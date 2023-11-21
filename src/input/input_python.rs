use std::borrow::Cow;
use std::str::from_utf8;

use pyo3::intern2;
use pyo3::prelude::*;
use pyo3::types::{
    PyBool, PyByteArray, PyBytes, PyDate, PyDateTime, PyDict, PyFloat, PyFrozenSet, PyInt, PyIterator, PyList,
    PyMapping, PySequence, PySet, PyString, PyTime, PyTuple, PyType,
};
#[cfg(not(PyPy))]
use pyo3::types::{PyDictItems, PyDictKeys, PyDictValues};

use speedate::MicrosecondsPrecisionOverflowBehavior;

use crate::errors::{AsLocItem, ErrorType, ErrorTypeDefaults, InputValue, LocItem, ValError, ValResult};
use crate::tools::{extract_i64, safe_repr};
use crate::validators::decimal::{create_decimal, get_decimal_type};
use crate::validators::Exactness;
use crate::{ArgsKwargs, PyMultiHostUrl, PyUrl};

use super::datetime::{
    bytes_as_date, bytes_as_datetime, bytes_as_time, bytes_as_timedelta, date_as_datetime, float_as_datetime,
    float_as_duration, float_as_time, int_as_datetime, int_as_duration, int_as_time, EitherDate, EitherDateTime,
    EitherTime,
};
use super::return_enums::ValidationMatch;
use super::shared::{
    decimal_as_int, float_as_int, get_enum_meta_object, int_as_bool, str_as_bool, str_as_float, str_as_int,
};
use super::{
    py_string_str, BorrowInput, EitherBytes, EitherFloat, EitherInt, EitherString, EitherTimedelta, GenericArguments,
    GenericIterable, GenericIterator, GenericMapping, Input, PyArgs,
};

#[cfg(not(PyPy))]
macro_rules! extract_dict_keys {
    ($py:expr, $obj:expr) => {
        $obj.downcast::<PyDictKeys>()
            .ok()
            .map(|v| PyIterator::from_object2(v).unwrap())
    };
}

#[cfg(PyPy)]
macro_rules! extract_dict_keys {
    ($py:expr, $obj:expr) => {
        if is_dict_keys_type($obj) {
            Some(PyIterator::from_object2($obj).unwrap())
        } else {
            None
        }
    };
}

#[cfg(not(PyPy))]
macro_rules! extract_dict_values {
    ($py:expr, $obj:expr) => {
        $obj.downcast::<PyDictValues>()
            .ok()
            .map(|v| PyIterator::from_object2(v).unwrap())
    };
}

#[cfg(PyPy)]
macro_rules! extract_dict_values {
    ($py:expr, $obj:expr) => {
        if is_dict_values_type($obj) {
            Some(PyIterator::from_object2($obj).unwrap())
        } else {
            None
        }
    };
}

#[cfg(not(PyPy))]
macro_rules! extract_dict_items {
    ($py:expr, $obj:expr) => {
        $obj.downcast::<PyDictItems>()
            .ok()
            .map(|v| PyIterator::from_object2(v).unwrap())
    };
}

#[cfg(PyPy)]
macro_rules! extract_dict_items {
    ($py:expr, $obj:expr) => {
        if is_dict_items_type($obj) {
            Some(PyIterator::from_object2($obj).unwrap())
        } else {
            None
        }
    };
}

impl<'a> Input<'a> for Py2<'a, PyAny> {
    fn as_error_value(&self) -> InputValue {
        InputValue::Python(self.clone().into())
    }

    fn identity(&self) -> Option<usize> {
        Some(self.as_ptr() as usize)
    }

    fn is_none(&self) -> bool {
        PyAnyMethods::is_none(self)
    }

    fn input_is_instance(&self, class: &Py2<'_, PyType>) -> Option<&Py2<'a, PyAny>> {
        if self.is_instance(class).unwrap_or(false) {
            Some(self)
        } else {
            None
        }
    }

    fn is_python(&self) -> bool {
        true
    }

    fn as_kwargs(&self, _py: Python<'a>) -> Option<Py2<'a, PyDict>> {
        self.downcast::<PyDict>().ok().map(Clone::clone)
    }

    fn input_is_subclass(&self, class: &Py2<'_, PyType>) -> PyResult<bool> {
        match self.downcast::<PyType>() {
            Ok(py_type) => py_type.is_subclass(class),
            Err(_) => Ok(false),
        }
    }

    fn input_as_url(&self) -> Option<PyUrl> {
        self.extract::<PyUrl>().ok()
    }

    fn input_as_multi_host_url(&self) -> Option<PyMultiHostUrl> {
        self.extract::<PyMultiHostUrl>().ok()
    }

    fn callable(&self) -> bool {
        self.is_callable()
    }

    fn validate_args(&'a self) -> ValResult<GenericArguments<'a>> {
        if let Ok(dict) = self.downcast::<PyDict>() {
            Ok(PyArgs::new(None, Some(dict.clone())).into())
        } else if let Ok(args_kwargs) = self.extract::<ArgsKwargs>() {
            let args = args_kwargs.args.attach_into(self.py());
            let kwargs = args_kwargs.kwargs.map(|d| d.attach_into(self.py()));
            Ok(PyArgs::new(Some(args), kwargs).into())
        } else if let Ok(tuple) = self.downcast::<PyTuple>() {
            Ok(PyArgs::new(Some(tuple.clone()), None).into())
        } else if let Ok(list) = self.downcast::<PyList>() {
            Ok(PyArgs::new(Some(list.to_tuple()), None).into())
        } else {
            Err(ValError::new(ErrorTypeDefaults::ArgumentsType, self))
        }
    }

    fn validate_dataclass_args(&'a self, class_name: &str) -> ValResult<GenericArguments<'a>> {
        if let Ok(dict) = self.downcast::<PyDict>() {
            Ok(PyArgs::new(None, Some(dict.clone())).into())
        } else if let Ok(args_kwargs) = self.extract::<ArgsKwargs>() {
            let args = args_kwargs.args.attach_into(self.py());
            let kwargs = args_kwargs.kwargs.map(|d| d.attach_into(self.py()));
            Ok(PyArgs::new(Some(args), kwargs).into())
        } else {
            let class_name = class_name.to_string();
            Err(ValError::new(
                ErrorType::DataclassType {
                    class_name,
                    context: None,
                },
                self,
            ))
        }
    }

    fn validate_str(
        &'a self,
        strict: bool,
        coerce_numbers_to_str: bool,
    ) -> ValResult<ValidationMatch<EitherString<'a>>> {
        if let Ok(py_str) = self.downcast_exact::<PyString>() {
            return Ok(ValidationMatch::exact(py_str.clone().into()));
        } else if let Ok(py_str) = self.downcast::<PyString>() {
            // force to a rust string to make sure behavior is consistent whether or not we go via a
            // rust string in StrConstrainedValidator - e.g. to_lower
            return Ok(ValidationMatch::strict(py_string_str(py_str)?.into()));
        }

        'lax: {
            if !strict {
                return if let Ok(bytes) = self.downcast::<PyBytes>() {
                    match from_utf8(bytes.as_bytes()) {
                        Ok(str) => Ok(str.into()),
                        Err(_) => Err(ValError::new(ErrorTypeDefaults::StringUnicode, self)),
                    }
                } else if let Ok(py_byte_array) = self.downcast::<PyByteArray>() {
                    // Safety: the gil is held while from_utf8 is running so py_byte_array is not mutated,
                    // and we immediately copy the bytes into a new Python string
                    match from_utf8(unsafe { py_byte_array.as_bytes() }) {
                        // Why Python not Rust? to avoid an unnecessary allocation on the Rust side, the
                        // final output needs to be Python anyway.
                        Ok(s) => Ok(PyString::new2(self.py(), s).into()),
                        Err(_) => Err(ValError::new(ErrorTypeDefaults::StringUnicode, self)),
                    }
                } else if coerce_numbers_to_str && !self.is_exact_instance_of::<PyBool>() && {
                    let py = self.py();
                    let decimal_type = get_decimal_type(py);

                    // only allow int, float, and decimal (not bool)
                    self.is_instance_of::<PyInt>()
                        || self.is_instance_of::<PyFloat>()
                        || self.is_instance(decimal_type).unwrap_or_default()
                } {
                    Ok(self.str()?.into())
                } else if let Some(enum_val) = maybe_as_enum(self) {
                    Ok(enum_val.str()?.into())
                } else {
                    break 'lax;
                }
                .map(ValidationMatch::lax);
            }
        }

        Err(ValError::new(ErrorTypeDefaults::StringType, self))
    }

    fn exact_str(&'a self) -> ValResult<EitherString<'a>> {
        if let Ok(py_str) = self.downcast_exact::<PyString>() {
            Ok(EitherString::Py(py_str.clone()))
        } else {
            Err(ValError::new(ErrorTypeDefaults::IntType, self))
        }
    }

    fn exact_int(&'a self) -> ValResult<EitherInt<'a>> {
        if self.is_exact_instance_of::<PyInt>() {
            Ok(EitherInt::Py(self.clone()))
        } else {
            Err(ValError::new(ErrorTypeDefaults::IntType, self))
        }
    }

    fn validate_bytes(&'a self, strict: bool) -> ValResult<ValidationMatch<EitherBytes<'a>>> {
        if let Ok(py_bytes) = self.downcast_exact::<PyBytes>() {
            return Ok(ValidationMatch::exact(py_bytes.into()));
        } else if let Ok(py_bytes) = self.downcast::<PyBytes>() {
            return Ok(ValidationMatch::strict(py_bytes.into()));
        }

        'lax: {
            if !strict {
                return if let Ok(py_str) = self.downcast::<PyString>() {
                    let str = py_string_str(py_str)?;
                    Ok(str.as_bytes().into())
                } else if let Ok(py_byte_array) = self.downcast::<PyByteArray>() {
                    Ok(py_byte_array.to_vec().into())
                } else {
                    break 'lax;
                }
                .map(ValidationMatch::lax);
            }
        }

        Err(ValError::new(ErrorTypeDefaults::BytesType, self))
    }

    fn validate_bool(&self, strict: bool) -> ValResult<ValidationMatch<bool>> {
        if let Ok(bool) = self.downcast::<PyBool>() {
            return Ok(ValidationMatch::exact(bool.is_true()));
        }

        if !strict {
            if let Some(cow_str) = maybe_as_string(self, ErrorTypeDefaults::BoolParsing)? {
                return str_as_bool(self, &cow_str).map(ValidationMatch::lax);
            } else if let Ok(int) = extract_i64(self) {
                return int_as_bool(self, int).map(ValidationMatch::lax);
            } else if let Ok(float) = self.extract::<f64>() {
                if let Ok(int) = float_as_int(self, float) {
                    return int
                        .as_bool()
                        .ok_or_else(|| ValError::new(ErrorTypeDefaults::BoolParsing, self))
                        .map(ValidationMatch::lax);
                };
            }
        }

        Err(ValError::new(ErrorTypeDefaults::BoolType, self))
    }

    fn validate_int(&'a self, strict: bool) -> ValResult<ValidationMatch<EitherInt<'a>>> {
        if self.is_exact_instance_of::<PyInt>() {
            return Ok(ValidationMatch::exact(EitherInt::Py(self.clone())));
        } else if self.is_instance_of::<PyInt>() {
            // bools are a subclass of int, so check for bool type in this specific case
            let exactness = if self.is_instance_of::<PyBool>() {
                if strict {
                    return Err(ValError::new(ErrorTypeDefaults::IntType, self));
                }
                Exactness::Lax
            } else {
                Exactness::Strict
            };

            // force to an int to upcast to a pure python int
            return EitherInt::upcast(self).map(|either_int| ValidationMatch::new(either_int, exactness));
        }

        'lax: {
            if !strict {
                return if let Some(cow_str) = maybe_as_string(self, ErrorTypeDefaults::IntParsing)? {
                    str_as_int(self, &cow_str)
                } else if self.is_exact_instance_of::<PyFloat>() {
                    float_as_int(self, self.extract::<f64>()?)
                } else if let Ok(decimal) = self.strict_decimal(self.py()) {
                    decimal_as_int(self, &decimal)
                } else if let Ok(float) = self.extract::<f64>() {
                    float_as_int(self, float)
                } else if let Some(enum_val) = maybe_as_enum(self) {
                    Ok(EitherInt::Py(enum_val))
                } else {
                    break 'lax;
                }
                .map(ValidationMatch::lax);
            }
        }

        Err(ValError::new(ErrorTypeDefaults::IntType, self))
    }

    fn validate_float(&'a self, strict: bool) -> ValResult<ValidationMatch<EitherFloat<'a>>> {
        if let Ok(float) = self.downcast_exact::<PyFloat>() {
            return Ok(ValidationMatch::exact(EitherFloat::Py(float.clone())));
        }

        if !strict {
            if let Some(cow_str) = maybe_as_string(self, ErrorTypeDefaults::FloatParsing)? {
                // checking for bytes and string is fast, so do this before isinstance(float)
                return str_as_float(self, &cow_str).map(ValidationMatch::lax);
            }
        }

        if let Ok(float) = self.extract::<f64>() {
            let exactness = if self.is_instance_of::<PyBool>() {
                if strict {
                    return Err(ValError::new(ErrorTypeDefaults::FloatType, self));
                }
                Exactness::Lax
            } else {
                Exactness::Strict
            };
            return Ok(ValidationMatch::new(EitherFloat::F64(float), exactness));
        }

        Err(ValError::new(ErrorTypeDefaults::FloatType, self))
    }

    fn strict_decimal(&'a self, py: Python<'a>) -> ValResult<Py2<'a, PyAny>> {
        let decimal_type = get_decimal_type(py);
        // Fast path for existing decimal objects
        if self.is_exact_instance(decimal_type) {
            return Ok(self.clone());
        }

        // Try subclasses of decimals, they will be upcast to Decimal
        if self.is_instance(decimal_type)? {
            return create_decimal(self, self);
        }

        Err(ValError::new(
            ErrorType::IsInstanceOf {
                class: decimal_type
                    .name()
                    .and_then(|any| any.extract())
                    .unwrap_or_else(|_| "Decimal".into()),
                context: None,
            },
            self,
        ))
    }

    fn lax_decimal(&'a self, py: Python<'a>) -> ValResult<Py2<'a, PyAny>> {
        let decimal_type = get_decimal_type(py);
        // Fast path for existing decimal objects
        if self.is_exact_instance(decimal_type) {
            return Ok(self.clone());
        }

        if self.is_instance_of::<PyString>() || (self.is_instance_of::<PyInt>() && !self.is_instance_of::<PyBool>()) {
            // checking isinstance for str / int / bool is fast compared to decimal / float
            create_decimal(self, self)
        } else if self.is_instance(decimal_type)? {
            // upcast subclasses to decimal
            return create_decimal(self, self);
        } else if self.is_instance_of::<PyFloat>() {
            create_decimal(self.str()?.as_any(), self)
        } else {
            Err(ValError::new(ErrorTypeDefaults::DecimalType, self))
        }
    }

    fn strict_dict(&'a self) -> ValResult<GenericMapping<'a>> {
        if let Ok(dict) = self.downcast::<PyDict>() {
            Ok(dict.clone().into())
        } else {
            Err(ValError::new(ErrorTypeDefaults::DictType, self))
        }
    }

    fn lax_dict(&'a self) -> ValResult<GenericMapping<'a>> {
        if let Ok(dict) = self.downcast::<PyDict>() {
            Ok(dict.clone().into())
        } else if let Ok(mapping) = self.downcast::<PyMapping>() {
            Ok(mapping.clone().into())
        } else {
            Err(ValError::new(ErrorTypeDefaults::DictType, self))
        }
    }

    fn validate_model_fields(&'a self, strict: bool, from_attributes: bool) -> ValResult<GenericMapping<'a>> {
        if from_attributes {
            // if from_attributes, first try a dict, then mapping then from_attributes
            if let Ok(dict) = self.downcast::<PyDict>() {
                return Ok(dict.clone().into());
            } else if !strict {
                if let Ok(mapping) = self.downcast::<PyMapping>() {
                    return Ok(mapping.clone().into());
                }
            }

            if from_attributes_applicable(self) {
                Ok(self.clone().into())
            } else if let Ok((obj, kwargs)) = self.extract() {
                if from_attributes_applicable(&obj) {
                    Ok(GenericMapping::PyGetAttr(obj, Some(kwargs)))
                } else {
                    Err(ValError::new(ErrorTypeDefaults::ModelAttributesType, self))
                }
            } else {
                // note the error here gives a hint about from_attributes
                Err(ValError::new(ErrorTypeDefaults::ModelAttributesType, self))
            }
        } else {
            // otherwise we just call back to validate_dict if from_mapping is allowed, note that errors in this
            // case (correctly) won't hint about from_attributes
            self.validate_dict(strict)
        }
    }

    fn strict_list(&'a self) -> ValResult<GenericIterable<'a>> {
        match self.lax_list()? {
            GenericIterable::List(iter) => Ok(GenericIterable::List(iter)),
            _ => Err(ValError::new(ErrorTypeDefaults::ListType, self)),
        }
    }

    fn lax_list(&'a self) -> ValResult<GenericIterable<'a>> {
        match self
            .extract_generic_iterable()
            .map_err(|_| ValError::new(ErrorTypeDefaults::ListType, self))?
        {
            GenericIterable::PyString(_)
            | GenericIterable::Bytes(_)
            | GenericIterable::Dict(_)
            | GenericIterable::Mapping(_) => Err(ValError::new(ErrorTypeDefaults::ListType, self)),
            other => Ok(other),
        }
    }

    fn strict_tuple(&'a self) -> ValResult<GenericIterable<'a>> {
        match self.lax_tuple()? {
            GenericIterable::Tuple(iter) => Ok(GenericIterable::Tuple(iter)),
            _ => Err(ValError::new(ErrorTypeDefaults::TupleType, self)),
        }
    }

    fn lax_tuple(&'a self) -> ValResult<GenericIterable<'a>> {
        match self
            .extract_generic_iterable()
            .map_err(|_| ValError::new(ErrorTypeDefaults::TupleType, self))?
        {
            GenericIterable::PyString(_)
            | GenericIterable::Bytes(_)
            | GenericIterable::Dict(_)
            | GenericIterable::Mapping(_) => Err(ValError::new(ErrorTypeDefaults::TupleType, self)),
            other => Ok(other),
        }
    }

    fn strict_set(&'a self) -> ValResult<GenericIterable<'a>> {
        match self.lax_set()? {
            GenericIterable::Set(iter) => Ok(GenericIterable::Set(iter)),
            _ => Err(ValError::new(ErrorTypeDefaults::SetType, self)),
        }
    }

    fn lax_set(&'a self) -> ValResult<GenericIterable<'a>> {
        match self
            .extract_generic_iterable()
            .map_err(|_| ValError::new(ErrorTypeDefaults::SetType, self))?
        {
            GenericIterable::PyString(_)
            | GenericIterable::Bytes(_)
            | GenericIterable::Dict(_)
            | GenericIterable::Mapping(_) => Err(ValError::new(ErrorTypeDefaults::SetType, self)),
            other => Ok(other),
        }
    }

    fn strict_frozenset(&'a self) -> ValResult<GenericIterable<'a>> {
        match self.lax_frozenset()? {
            GenericIterable::FrozenSet(iter) => Ok(GenericIterable::FrozenSet(iter)),
            _ => Err(ValError::new(ErrorTypeDefaults::FrozenSetType, self)),
        }
    }

    fn lax_frozenset(&'a self) -> ValResult<GenericIterable<'a>> {
        match self
            .extract_generic_iterable()
            .map_err(|_| ValError::new(ErrorTypeDefaults::FrozenSetType, self))?
        {
            GenericIterable::PyString(_)
            | GenericIterable::Bytes(_)
            | GenericIterable::Dict(_)
            | GenericIterable::Mapping(_) => Err(ValError::new(ErrorTypeDefaults::FrozenSetType, self)),
            other => Ok(other),
        }
    }

    fn extract_generic_iterable(&'a self) -> ValResult<GenericIterable<'a>> {
        // Handle concrete non-overlapping types first, then abstract types
        if let Ok(iterable) = self.downcast::<PyList>() {
            Ok(GenericIterable::List(iterable.clone()))
        } else if let Ok(iterable) = self.downcast::<PyTuple>() {
            Ok(GenericIterable::Tuple(iterable.clone()))
        } else if let Ok(iterable) = self.downcast::<PySet>() {
            Ok(GenericIterable::Set(iterable.clone()))
        } else if let Ok(iterable) = self.downcast::<PyFrozenSet>() {
            Ok(GenericIterable::FrozenSet(iterable.clone()))
        } else if let Ok(iterable) = self.downcast::<PyDict>() {
            Ok(GenericIterable::Dict(iterable.clone()))
        } else if let Some(iterable) = extract_dict_keys!(self.py(), self) {
            Ok(GenericIterable::DictKeys(iterable))
        } else if let Some(iterable) = extract_dict_values!(self.py(), self) {
            Ok(GenericIterable::DictValues(iterable))
        } else if let Some(iterable) = extract_dict_items!(self.py(), self) {
            Ok(GenericIterable::DictItems(iterable))
        } else if let Ok(iterable) = self.downcast::<PyMapping>() {
            Ok(GenericIterable::Mapping(iterable.clone()))
        } else if let Ok(iterable) = self.downcast::<PyString>() {
            Ok(GenericIterable::PyString(iterable.clone()))
        } else if let Ok(iterable) = self.downcast::<PyBytes>() {
            Ok(GenericIterable::Bytes(iterable.clone()))
        } else if let Ok(iterable) = self.downcast::<PyByteArray>() {
            Ok(GenericIterable::PyByteArray(iterable.clone()))
        } else if let Ok(iterable) = self.downcast::<PySequence>() {
            Ok(GenericIterable::Sequence(iterable.clone()))
        } else if let Ok(iterable) = self.iter() {
            Ok(GenericIterable::Iterator(iterable))
        } else {
            Err(ValError::new(ErrorTypeDefaults::IterableType, self))
        }
    }

    fn validate_iter(&self) -> ValResult<GenericIterator> {
        if self.iter().is_ok() {
            Ok(self.into())
        } else {
            Err(ValError::new(ErrorTypeDefaults::IterableType, self))
        }
    }

    fn validate_date(&self, strict: bool) -> ValResult<ValidationMatch<EitherDate>> {
        if let Ok(date) = self.downcast_exact::<PyDate>() {
            Ok(ValidationMatch::exact(date.clone().into()))
        } else if self.is_instance_of::<PyDateTime>() {
            // have to check if it's a datetime first, otherwise the line below converts to a date
            // even if we later try coercion from a datetime, we don't want to return a datetime now
            Err(ValError::new(ErrorTypeDefaults::DateType, self))
        } else if let Ok(date) = self.downcast::<PyDate>() {
            Ok(ValidationMatch::strict(date.clone().into()))
        } else if let Some(bytes) = {
            if strict {
                None
            } else if let Ok(py_str) = self.downcast::<PyString>() {
                let str = py_string_str(py_str)?;
                Some(str.as_bytes())
            } else if let Ok(py_bytes) = self.downcast::<PyBytes>() {
                Some(py_bytes.as_bytes())
            } else {
                None
            }
        } {
            bytes_as_date(self, bytes).map(ValidationMatch::lax)
        } else {
            Err(ValError::new(ErrorTypeDefaults::DateType, self))
        }
    }

    fn validate_time(
        &self,
        strict: bool,
        microseconds_overflow_behavior: MicrosecondsPrecisionOverflowBehavior,
    ) -> ValResult<ValidationMatch<EitherTime>> {
        if let Ok(time) = self.downcast_exact::<PyTime>() {
            return Ok(ValidationMatch::exact(time.clone().into()));
        } else if let Ok(time) = self.downcast::<PyTime>() {
            return Ok(ValidationMatch::strict(time.clone().into()));
        }

        'lax: {
            if !strict {
                return if let Ok(py_str) = self.downcast::<PyString>() {
                    let str = py_string_str(py_str)?;
                    bytes_as_time(self, str.as_bytes(), microseconds_overflow_behavior)
                } else if let Ok(py_bytes) = self.downcast::<PyBytes>() {
                    bytes_as_time(self, py_bytes.as_bytes(), microseconds_overflow_behavior)
                } else if self.is_exact_instance_of::<PyBool>() {
                    Err(ValError::new(ErrorTypeDefaults::TimeType, self))
                } else if let Ok(int) = extract_i64(self) {
                    int_as_time(self, int, 0)
                } else if let Ok(float) = self.extract::<f64>() {
                    float_as_time(self, float)
                } else {
                    break 'lax;
                }
                .map(ValidationMatch::lax);
            }
        }

        Err(ValError::new(ErrorTypeDefaults::TimeType, self))
    }

    fn validate_datetime(
        &self,
        strict: bool,
        microseconds_overflow_behavior: MicrosecondsPrecisionOverflowBehavior,
    ) -> ValResult<ValidationMatch<EitherDateTime>> {
        if let Ok(dt) = self.downcast_exact::<PyDateTime>() {
            return Ok(ValidationMatch::exact(dt.clone().into()));
        } else if let Ok(dt) = self.downcast::<PyDateTime>() {
            return Ok(ValidationMatch::strict(dt.clone().into()));
        }

        'lax: {
            if !strict {
                return if let Ok(py_str) = self.downcast::<PyString>() {
                    let str = py_string_str(py_str)?;
                    bytes_as_datetime(self, str.as_bytes(), microseconds_overflow_behavior)
                } else if let Ok(py_bytes) = self.downcast::<PyBytes>() {
                    bytes_as_datetime(self, py_bytes.as_bytes(), microseconds_overflow_behavior)
                } else if self.is_exact_instance_of::<PyBool>() {
                    Err(ValError::new(ErrorTypeDefaults::DatetimeType, self))
                } else if let Ok(int) = extract_i64(self) {
                    int_as_datetime(self, int, 0)
                } else if let Ok(float) = self.extract::<f64>() {
                    float_as_datetime(self, float)
                } else if let Ok(date) = self.downcast::<PyDate>() {
                    Ok(date_as_datetime(date)?)
                } else {
                    break 'lax;
                }
                .map(ValidationMatch::lax);
            }
        }

        Err(ValError::new(ErrorTypeDefaults::DatetimeType, self))
    }

    fn validate_timedelta(
        &self,
        strict: bool,
        microseconds_overflow_behavior: MicrosecondsPrecisionOverflowBehavior,
    ) -> ValResult<ValidationMatch<EitherTimedelta>> {
        if let Ok(either_dt) = EitherTimedelta::try_from(self) {
            let exactness = if matches!(either_dt, EitherTimedelta::PyExact(_)) {
                Exactness::Exact
            } else {
                Exactness::Strict
            };
            return Ok(ValidationMatch::new(either_dt, exactness));
        }

        'lax: {
            if !strict {
                return if let Ok(py_str) = self.downcast::<PyString>() {
                    let str = py_string_str(py_str)?;
                    bytes_as_timedelta(self, str.as_bytes(), microseconds_overflow_behavior)
                } else if let Ok(py_bytes) = self.downcast::<PyBytes>() {
                    bytes_as_timedelta(self, py_bytes.as_bytes(), microseconds_overflow_behavior)
                } else if let Ok(int) = extract_i64(self) {
                    Ok(int_as_duration(self, int)?.into())
                } else if let Ok(float) = self.extract::<f64>() {
                    Ok(float_as_duration(self, float)?.into())
                } else {
                    break 'lax;
                }
                .map(ValidationMatch::lax);
            }
        }

        Err(ValError::new(ErrorTypeDefaults::TimeDeltaType, self))
    }
}

impl AsLocItem for Py2<'_, PyAny> {
    fn as_loc_item(&self) -> LocItem {
        if let Ok(py_str) = self.downcast::<PyString>() {
            py_str.to_string_lossy().as_ref().into()
        } else if let Ok(key_int) = extract_i64(self) {
            key_int.into()
        } else {
            safe_repr(self).into_owned().into()
        }
    }
}

impl BorrowInput for Py2<'_, PyAny> {
    type Input<'a> = Py2<'a, PyAny> where Self: 'a;
    fn borrow_input(&self) -> &Self::Input<'_> {
        self
    }
}

impl BorrowInput for Py2Borrowed<'_, '_, PyAny> {
    type Input<'a> = Py2<'a, PyAny> where Self: 'a;
    fn borrow_input(&self) -> &Self::Input<'_> {
        self
    }
}

/// Best effort check of whether it's likely to make sense to inspect obj for attributes and iterate over it
/// with `obj.dir()`
fn from_attributes_applicable(obj: &Py2<'_, PyAny>) -> bool {
    let Some(module_name) = obj
        .get_type()
        .getattr(intern2!(obj.py(), "__module__"))
        .ok()
        .and_then(|module_name| module_name.downcast_into::<PyString>().ok())
    else {
        return false;
    };
    // I don't think it's a very good list at all! But it doesn't have to be at perfect, it just needs to avoid
    // the most egregious foot guns, it's mostly just to catch "builtins"
    // still happy to add more or do something completely different if anyone has a better idea???
    // dbg!(obj, module_name);
    !matches!(module_name.to_str(), Ok("builtins" | "datetime" | "collections"))
}

/// Utility for extracting a string from a PyAny, if possible.
fn maybe_as_string<'a>(v: &'a Py2<'_, PyAny>, unicode_error: ErrorType) -> ValResult<Option<Cow<'a, str>>> {
    if let Ok(py_string) = v.downcast::<PyString>() {
        let str = py_string_str(py_string)?;
        Ok(Some(Cow::Borrowed(str)))
    } else if let Ok(bytes) = v.downcast::<PyBytes>() {
        match from_utf8(bytes.as_bytes()) {
            Ok(s) => Ok(Some(Cow::Owned(s.to_string()))),
            Err(_) => Err(ValError::new(unicode_error, v)),
        }
    } else {
        Ok(None)
    }
}

/// Utility for extracting an enum value, if possible.
fn maybe_as_enum<'py>(v: &Py2<'py, PyAny>) -> Option<Py2<'py, PyAny>> {
    let py = v.py();
    let enum_meta_object = get_enum_meta_object(py);
    let meta_type = v.get_type().get_type();
    if meta_type.is(enum_meta_object) {
        v.getattr(intern2!(py, "value")).ok()
    } else {
        None
    }
}

#[cfg(PyPy)]
static DICT_KEYS_TYPE: pyo3::sync::GILOnceCell<Py<PyType>> = pyo3::sync::GILOnceCell::new();

#[cfg(PyPy)]
fn is_dict_keys_type(v: &Py2<'_, PyAny>) -> bool {
    let py = v.py();
    let keys_type = DICT_KEYS_TYPE
        .get_or_init(py, || {
            py.eval("type({}.keys())", None, None)
                .unwrap()
                .downcast::<PyType>()
                .unwrap()
                .into()
        })
        .attach(py);
    v.is_instance(keys_type).unwrap_or(false)
}

#[cfg(PyPy)]
static DICT_VALUES_TYPE: pyo3::sync::GILOnceCell<Py<PyType>> = pyo3::sync::GILOnceCell::new();

#[cfg(PyPy)]
fn is_dict_values_type(v: &Py2<'_, PyAny>) -> bool {
    let py = v.py();
    let values_type = DICT_VALUES_TYPE
        .get_or_init(py, || {
            py.eval("type({}.values())", None, None)
                .unwrap()
                .downcast::<PyType>()
                .unwrap()
                .into()
        })
        .attach(py);
    v.is_instance(values_type).unwrap_or(false)
}

#[cfg(PyPy)]
static DICT_ITEMS_TYPE: pyo3::sync::GILOnceCell<Py<PyType>> = pyo3::sync::GILOnceCell::new();

#[cfg(PyPy)]
fn is_dict_items_type(v: &Py2<'_, PyAny>) -> bool {
    let py = v.py();
    let items_type = DICT_ITEMS_TYPE
        .get_or_init(py, || {
            py.eval("type({}.items())", None, None)
                .unwrap()
                .downcast::<PyType>()
                .unwrap()
                .into()
        })
        .attach(py);
    v.is_instance(items_type).unwrap_or(false)
}
