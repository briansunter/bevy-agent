//! Serialize gameplay state without JSON's lossy non-finite-to-null conversion.

use serde::Serialize;
use serde::ser::{self, Error, Serializer};

pub(crate) fn to_value<T: Serialize + ?Sized>(
    value: &T,
) -> Result<serde_json::Value, serde_json::Error> {
    Checked(value).serialize(serde_json::value::Serializer)
}

struct Checked<'a, T: ?Sized>(&'a T);
impl<T: Serialize + ?Sized> Serialize for Checked<'_, T> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.0.serialize(Finite(serializer))
    }
}
struct Finite<S>(S);
struct Compound<S>(S);

macro_rules! forward {
    ($($method:ident($($arg:ident: $ty:ty),*)),* $(,)?) => {
        $(fn $method(self, $($arg: $ty),*) -> Result<Self::Ok, Self::Error> {
            self.0.$method($($arg),*)
        })*
    };
}

impl<S: Serializer> Serializer for Finite<S> {
    type Ok = S::Ok;
    type Error = S::Error;
    type SerializeSeq = Compound<S::SerializeSeq>;
    type SerializeTuple = Compound<S::SerializeTuple>;
    type SerializeTupleStruct = Compound<S::SerializeTupleStruct>;
    type SerializeTupleVariant = Compound<S::SerializeTupleVariant>;
    type SerializeMap = Compound<S::SerializeMap>;
    type SerializeStruct = Compound<S::SerializeStruct>;
    type SerializeStructVariant = Compound<S::SerializeStructVariant>;

    forward! {
        serialize_bool(value: bool), serialize_i8(value: i8), serialize_i16(value: i16),
        serialize_i32(value: i32), serialize_i64(value: i64), serialize_i128(value: i128),
        serialize_u8(value: u8), serialize_u16(value: u16), serialize_u32(value: u32),
        serialize_u64(value: u64), serialize_u128(value: u128), serialize_char(value: char),
        serialize_str(value: &str), serialize_bytes(value: &[u8]), serialize_none(),
        serialize_unit(), serialize_unit_struct(name: &'static str),
        serialize_unit_variant(name: &'static str, index: u32, variant: &'static str)
    }
    fn serialize_f32(self, value: f32) -> Result<Self::Ok, Self::Error> {
        if !value.is_finite() {
            return Err(S::Error::custom("non-finite gameplay float"));
        }
        self.0.serialize_f32(value)
    }
    fn serialize_f64(self, value: f64) -> Result<Self::Ok, Self::Error> {
        if !value.is_finite() {
            return Err(S::Error::custom("non-finite gameplay float"));
        }
        self.0.serialize_f64(value)
    }
    fn serialize_some<T: Serialize + ?Sized>(self, value: &T) -> Result<Self::Ok, Self::Error> {
        self.0.serialize_some(&Checked(value))
    }
    fn serialize_newtype_struct<T: Serialize + ?Sized>(
        self,
        name: &'static str,
        value: &T,
    ) -> Result<Self::Ok, Self::Error> {
        self.0.serialize_newtype_struct(name, &Checked(value))
    }
    fn serialize_newtype_variant<T: Serialize + ?Sized>(
        self,
        name: &'static str,
        index: u32,
        variant: &'static str,
        value: &T,
    ) -> Result<Self::Ok, Self::Error> {
        self.0
            .serialize_newtype_variant(name, index, variant, &Checked(value))
    }
    fn serialize_seq(self, len: Option<usize>) -> Result<Self::SerializeSeq, Self::Error> {
        self.0.serialize_seq(len).map(Compound)
    }
    fn serialize_tuple(self, len: usize) -> Result<Self::SerializeTuple, Self::Error> {
        self.0.serialize_tuple(len).map(Compound)
    }
    fn serialize_tuple_struct(
        self,
        name: &'static str,
        len: usize,
    ) -> Result<Self::SerializeTupleStruct, Self::Error> {
        self.0.serialize_tuple_struct(name, len).map(Compound)
    }
    fn serialize_tuple_variant(
        self,
        name: &'static str,
        index: u32,
        variant: &'static str,
        len: usize,
    ) -> Result<Self::SerializeTupleVariant, Self::Error> {
        self.0
            .serialize_tuple_variant(name, index, variant, len)
            .map(Compound)
    }
    fn serialize_map(self, len: Option<usize>) -> Result<Self::SerializeMap, Self::Error> {
        self.0.serialize_map(len).map(Compound)
    }
    fn serialize_struct(
        self,
        name: &'static str,
        len: usize,
    ) -> Result<Self::SerializeStruct, Self::Error> {
        self.0.serialize_struct(name, len).map(Compound)
    }
    fn serialize_struct_variant(
        self,
        name: &'static str,
        index: u32,
        variant: &'static str,
        len: usize,
    ) -> Result<Self::SerializeStructVariant, Self::Error> {
        self.0
            .serialize_struct_variant(name, index, variant, len)
            .map(Compound)
    }
    fn is_human_readable(&self) -> bool {
        self.0.is_human_readable()
    }
}

macro_rules! elements {
    ($trait:ident, $method:ident) => {
        impl<S: ser::$trait> ser::$trait for Compound<S> {
            type Ok = S::Ok;
            type Error = S::Error;
            fn $method<T: Serialize + ?Sized>(&mut self, value: &T) -> Result<(), Self::Error> {
                self.0.$method(&Checked(value))
            }
            fn end(self) -> Result<Self::Ok, Self::Error> {
                self.0.end()
            }
        }
    };
}
elements!(SerializeSeq, serialize_element);
elements!(SerializeTuple, serialize_element);
elements!(SerializeTupleStruct, serialize_field);
elements!(SerializeTupleVariant, serialize_field);
impl<S: ser::SerializeMap> ser::SerializeMap for Compound<S> {
    type Ok = S::Ok;
    type Error = S::Error;
    fn serialize_key<T: Serialize + ?Sized>(&mut self, key: &T) -> Result<(), Self::Error> {
        self.0.serialize_key(&Checked(key))
    }
    fn serialize_value<T: Serialize + ?Sized>(&mut self, value: &T) -> Result<(), Self::Error> {
        self.0.serialize_value(&Checked(value))
    }
    fn end(self) -> Result<Self::Ok, Self::Error> {
        self.0.end()
    }
}
macro_rules! fields {
    ($trait:ident) => {
        impl<S: ser::$trait> ser::$trait for Compound<S> {
            type Ok = S::Ok;
            type Error = S::Error;
            fn serialize_field<T: Serialize + ?Sized>(
                &mut self,
                key: &'static str,
                value: &T,
            ) -> Result<(), Self::Error> {
                self.0.serialize_field(key, &Checked(value))
            }
            fn skip_field(&mut self, key: &'static str) -> Result<(), Self::Error> {
                self.0.skip_field(key)
            }
            fn end(self) -> Result<Self::Ok, Self::Error> {
                self.0.end()
            }
        }
    };
}
fields!(SerializeStruct);
fields!(SerializeStructVariant);
