use ahash::AHashSet;
use pyo3::intern;
use pyo3::prelude::*;
use pyo3::types::{PyDict, PyList, PyString, PyTuple};

use crate::build_tools::{py_error, schema_or_config_same, SchemaDict};
use crate::errors::{ErrorKind, ValError, ValLineError, ValResult};
use crate::input::{GenericArguments, Input};
use crate::lookup_key::LookupKey;
use crate::recursion_guard::RecursionGuard;
use crate::SchemaError;

use super::{build_validator, BuildContext, BuildValidator, CombinedValidator, Extra, Validator};

#[derive(Debug, Clone)]
struct Argument {
    positional: bool,
    name: String,
    kw_lookup_key: Option<LookupKey>,
    kwarg_key: Option<Py<PyString>>,
    default: Option<PyObject>,
    default_factory: Option<PyObject>,
    validator: CombinedValidator,
}

#[derive(Debug, Clone)]
pub struct ArgumentsValidator {
    arguments: Vec<Argument>,
    positional_args_count: usize,
    var_args_validator: Option<Box<CombinedValidator>>,
    var_kwargs_validator: Option<Box<CombinedValidator>>,
}

impl BuildValidator for ArgumentsValidator {
    const EXPECTED_TYPE: &'static str = "arguments";

    fn build(
        schema: &PyDict,
        config: Option<&PyDict>,
        build_context: &mut BuildContext,
    ) -> PyResult<CombinedValidator> {
        let py = schema.py();

        let populate_by_name = schema_or_config_same(schema, config, intern!(py, "populate_by_name"))?.unwrap_or(false);

        let arguments_list: &PyList = schema.get_as_req(intern!(py, "arguments_schema"))?;
        let mut arguments: Vec<Argument> = Vec::with_capacity(arguments_list.len());

        let mut positional_args_count = 0;
        let mut had_default_arg = false;

        for (arg_index, arg) in arguments_list.iter().enumerate() {
            let arg: &PyDict = arg.cast_as()?;

            let name: String = arg.get_as_req(intern!(py, "name"))?;
            let mode: &str = arg.get_as_req(intern!(py, "mode"))?;
            let positional = mode == "positional_only" || mode == "positional_or_keyword";
            if positional {
                positional_args_count = arg_index + 1;
            }

            let mut kw_lookup_key = None;
            let mut kwarg_key = None;
            if mode == "keyword_only" || mode == "positional_or_keyword" {
                kw_lookup_key = match arg.get_item(intern!(py, "alias")) {
                    Some(alias) => {
                        let alt_alias = if populate_by_name { Some(name.as_str()) } else { None };
                        Some(LookupKey::from_py(py, alias, alt_alias)?)
                    }
                    None => Some(LookupKey::from_string(py, &name)),
                };
                kwarg_key = Some(PyString::intern(py, &name).into());
            }

            let schema: &PyAny = arg
                .get_as_req(intern!(py, "schema"))
                .map_err(|err| SchemaError::new_err(format!("Argument \"{}\":\n  {}", name, err)))?;

            let (validator, _) = build_validator(schema, config, build_context)?;

            let default = arg.get_as(intern!(py, "default"))?;
            let default_factory = arg.get_as(intern!(py, "default_factory"))?;
            if default.is_some() && default_factory.is_some() {
                return py_error!("'default' and 'default_factory' cannot be used together");
            } else if had_default_arg && (default.is_none() && default_factory.is_none()) {
                return py_error!("Non-default argument follows default argument");
            } else if default.is_some() || default_factory.is_some() {
                had_default_arg = true;
            }
            arguments.push(Argument {
                positional,
                kw_lookup_key,
                name,
                kwarg_key,
                default,
                default_factory,
                validator,
            });
        }

        Ok(Self {
            arguments,
            positional_args_count,
            var_args_validator: match schema.get_item(intern!(py, "var_args_schema")) {
                Some(v) => Some(Box::new(build_validator(v, config, build_context)?.0)),
                None => None,
            },
            var_kwargs_validator: match schema.get_item(intern!(py, "var_kwargs_schema")) {
                Some(v) => Some(Box::new(build_validator(v, config, build_context)?.0)),
                None => None,
            },
        }
        .into())
    }
}

macro_rules! py_get {
    ($obj:ident, $index:ident) => {
        $obj.get_item($index).ok()
    };
}

macro_rules! py_slice {
    ($obj:ident, $from:expr, $to:expr) => {
        $obj.get_slice($from, $to)
    };
}

macro_rules! json_get {
    ($obj:ident, $index:ident) => {
        $obj.get($index)
    };
}

macro_rules! json_slice {
    ($obj:ident, $from:expr, $to:expr) => {
        $obj[$from..$to]
    };
}

impl Validator for ArgumentsValidator {
    fn validate<'s, 'data>(
        &'s self,
        py: Python<'data>,
        input: &'data impl Input<'data>,
        extra: &Extra,
        slots: &'data [CombinedValidator],
        recursion_guard: &'s mut RecursionGuard,
    ) -> ValResult<'data, PyObject> {
        let args = input.validate_args()?;

        let mut output_args: Vec<PyObject> = Vec::with_capacity(self.positional_args_count);
        let output_kwargs = PyDict::new(py);
        let mut errors: Vec<ValLineError> = Vec::new();
        let mut used_kwargs: AHashSet<&str> = AHashSet::with_capacity(self.arguments.len());

        macro_rules! process {
            ($args:ident, $get_method:ident, $get_macro:ident, $slice_macro:ident) => {{
                // go through arguments getting the value from args or kwargs and validating it
                for (index, argument_info) in self.arguments.iter().enumerate() {
                    let mut pos_value = None;
                    if let Some(args) = $args.args {
                        if argument_info.positional {
                            pos_value = $get_macro!(args, index);
                        }
                    }
                    let mut kw_value = None;
                    if let Some(kwargs) = $args.kwargs {
                        if let Some(ref lookup_key) = argument_info.kw_lookup_key {
                            if let Some((key, value)) = lookup_key.$get_method(kwargs)? {
                                used_kwargs.insert(key);
                                kw_value = Some(value);
                            }
                        }
                    }

                    match (pos_value, kw_value) {
                        (Some(_), Some(kw_value)) => {
                            errors.push(ValLineError::new_with_loc(
                                ErrorKind::MultipleArgumentValues,
                                kw_value,
                                argument_info.name.clone(),
                            ));
                        }
                        (Some(pos_value), None) => {
                            match argument_info
                                .validator
                                .validate(py, pos_value, extra, slots, recursion_guard)
                            {
                                Ok(value) => output_args.push(value),
                                Err(ValError::LineErrors(line_errors)) => {
                                    errors.extend(line_errors.into_iter().map(|err| err.with_outer_location(index.into())));
                                }
                                Err(err) => return Err(err),
                            }
                        }
                        (None, Some(kw_value)) => {
                            match argument_info
                                .validator
                                .validate(py, kw_value, extra, slots, recursion_guard)
                            {
                                Ok(value) => output_kwargs.set_item(argument_info.kwarg_key.as_ref().unwrap(), value)?,
                                Err(ValError::LineErrors(line_errors)) => {
                                    errors.extend(
                                        line_errors
                                            .into_iter()
                                            .map(|err| err.with_outer_location(argument_info.name.clone().into())),
                                    );
                                }
                                Err(err) => return Err(err),
                            }
                        }
                        (None, None) => {
                            if let Some(ref default) = argument_info.default {
                                if let Some(ref kwarg_key) = argument_info.kwarg_key {
                                    output_kwargs.set_item(kwarg_key, default)?;
                                } else {
                                    output_args.push(default.clone_ref(py));
                                }
                            } else if let Some(ref default_factory) = argument_info.default_factory {
                                let default = default_factory.call0(py)?;
                                if let Some(ref kwarg_key) = argument_info.kwarg_key {
                                    output_kwargs.set_item(kwarg_key, default)?;
                                } else {
                                    output_args.push(default);
                                }
                            } else if argument_info.kwarg_key.is_some() {
                                errors.push(ValLineError::new_with_loc(
                                    ErrorKind::MissingKeywordArgument,
                                    input,
                                    argument_info.name.clone(),
                                ));
                            } else {
                                errors.push(ValLineError::new_with_loc(ErrorKind::MissingPositionalArgument, input, index));
                            };
                        }
                    }
                }
                // if there are args check any where index > positional_args_count since they won't have been checked yet
                if let Some(args) = $args.args {
                    let len = args.len();
                    if len > self.positional_args_count {
                        if let Some(ref validator) = self.var_args_validator {
                            for (index, item) in $slice_macro!(args, self.positional_args_count, len).iter().enumerate() {
                                match validator.validate(py, item, extra, slots, recursion_guard) {
                                    Ok(value) => output_args.push(value),
                                    Err(ValError::LineErrors(line_errors)) => {
                                        errors.extend(
                                            line_errors
                                                .into_iter()
                                                .map(|err| err.with_outer_location((index + self.positional_args_count).into())),
                                        );
                                    }
                                    Err(err) => return Err(err),
                                }
                            }
                        } else {
                            for (index, item) in $slice_macro!(args, self.positional_args_count, len).iter().enumerate() {
                                errors.push(ValLineError::new_with_loc(
                                    ErrorKind::UnexpectedPositionalArgument,
                                    item,
                                    index + self.positional_args_count,
                                ));
                            }
                        }
                    }
                }
                // if there are kwargs check any that haven't been processed yet
                if let Some(kwargs) = $args.kwargs {
                    for (raw_key, value) in kwargs.iter() {
                        let key = match raw_key.strict_str(py) {
                            Ok(k) => k,
                            Err(ValError::LineErrors(line_errors)) => {
                                for err in line_errors {
                                    errors.push(
                                        err.with_outer_location(raw_key.as_loc_item(py))
                                            .with_kind(ErrorKind::InvalidKey),
                                    );
                                }
                                continue;
                            }
                            Err(err) => return Err(err),
                        };
                        if !used_kwargs.contains(key.to_string_lossy().as_ref()) {
                            match self.var_kwargs_validator {
                                Some(ref validator) => match validator.validate(py, value, extra, slots, recursion_guard) {
                                    Ok(value) => output_kwargs.set_item(key, value)?,
                                    Err(ValError::LineErrors(line_errors)) => {
                                        for err in line_errors {
                                            errors.push(err.with_outer_location(raw_key.as_loc_item(py)));
                                        }
                                    }
                                    Err(err) => return Err(err),
                                },
                                None => {
                                    errors.push(ValLineError::new_with_loc(
                                        ErrorKind::UnexpectedKeywordArgument,
                                        value,
                                        raw_key.as_loc_item(py),
                                    ));
                                }
                            }
                        }
                    }
                }
            }};
        }
        match args {
            GenericArguments::Py(a) => process!(a, py_get_item, py_get, py_slice),
            GenericArguments::Json(a) => process!(a, json_get, json_get, json_slice),
        }
        if !errors.is_empty() {
            Err(ValError::LineErrors(errors))
        } else {
            Ok((PyTuple::new(py, output_args), output_kwargs).to_object(py))
        }
    }

    fn get_name(&self) -> &str {
        Self::EXPECTED_TYPE
    }
}
