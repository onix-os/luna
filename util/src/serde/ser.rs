use std::fmt;

use luna::{Context, Table, Value};
use serde::ser;
use thiserror::Error;

use super::markers::{none, unit};

#[derive(Debug, Error)]
#[error("{0}")]
pub struct Error(String);

impl ser::Error for Error {
    fn custom<T: fmt::Display>(msg: T) -> Self {
        Error(msg.to_string())
    }
}

#[derive(Debug, Copy, Clone)]
#[non_exhaustive]
#[derive(Default)]
pub struct Options {
    /// If true, serialize the special `none` marker instead of `nil`.
    pub serialize_none: bool,
}

impl Options {
    pub fn serialize_none(mut self, enabled: bool) -> Self {
        self.serialize_none = enabled;
        self
    }
}

pub fn to_value<'gc, T: ser::Serialize>(ctx: Context<'gc>, value: &T) -> Result<Value<'gc>, Error> {
    value.serialize(Serializer::new(ctx, Options::default()))
}

pub fn to_value_with<'gc, T: ser::Serialize>(
    ctx: Context<'gc>,
    value: &T,
    options: Options,
) -> Result<Value<'gc>, Error> {
    value.serialize(Serializer::new(ctx, options))
}

#[derive(Copy, Clone)]
pub struct Serializer<'gc> {
    ctx: Context<'gc>,
    options: Options,
}

impl<'gc> Serializer<'gc> {
    pub fn new(ctx: Context<'gc>, options: Options) -> Self {
        Self { ctx, options }
    }
}

impl<'gc> ser::Serializer for Serializer<'gc> {
    type Ok = Value<'gc>;
    type Error = Error;

    type SerializeSeq = SerializeSeq<'gc>;
    type SerializeTuple = SerializeSeq<'gc>;
    type SerializeTupleStruct = SerializeSeq<'gc>;
    type SerializeTupleVariant = SerializeTupleVariant<'gc>;
    type SerializeMap = SerializeMap<'gc>;
    type SerializeStruct = SerializeStruct<'gc>;
    type SerializeStructVariant = SerializeStructVariant<'gc>;

    fn serialize_bool(self, v: bool) -> Result<Value<'gc>, Error> {
        Ok(Value::Boolean(v))
    }

    fn serialize_i8(self, v: i8) -> Result<Value<'gc>, Error> {
        self.serialize_i64(v.into())
    }

    fn serialize_i16(self, v: i16) -> Result<Value<'gc>, Error> {
        self.serialize_i64(v.into())
    }

    fn serialize_i32(self, v: i32) -> Result<Value<'gc>, Error> {
        self.serialize_i64(v.into())
    }

    fn serialize_i64(self, v: i64) -> Result<Value<'gc>, Error> {
        Ok(Value::Integer(v))
    }

    fn serialize_u8(self, v: u8) -> Result<Value<'gc>, Error> {
        self.serialize_u64(v.into())
    }

    fn serialize_u16(self, v: u16) -> Result<Value<'gc>, Error> {
        self.serialize_u64(v.into())
    }

    fn serialize_u32(self, v: u32) -> Result<Value<'gc>, Error> {
        self.serialize_u64(v.into())
    }

    fn serialize_u64(self, v: u64) -> Result<Value<'gc>, Error> {
        if let Ok(i) = i64::try_from(v) {
            Ok(Value::Integer(i))
        } else {
            self.serialize_f64(v as f64)
        }
    }

    fn serialize_f32(self, v: f32) -> Result<Value<'gc>, Error> {
        self.serialize_f64(v.into())
    }

    fn serialize_f64(self, v: f64) -> Result<Value<'gc>, Error> {
        Ok(Value::Number(v))
    }

    fn serialize_char(self, v: char) -> Result<Value<'gc>, Error> {
        self.serialize_str(&v.to_string())
    }

    fn serialize_str(self, v: &str) -> Result<Value<'gc>, Error> {
        self.serialize_bytes(v.as_bytes())
    }

    fn serialize_bytes(self, v: &[u8]) -> Result<Value<'gc>, Error> {
        Ok(self.ctx.intern(v).into())
    }

    fn serialize_none(self) -> Result<Value<'gc>, Error> {
        Ok(if self.options.serialize_none {
            none(self.ctx).into()
        } else {
            Value::Nil
        })
    }

    fn serialize_some<T: ?Sized>(self, value: &T) -> Result<Value<'gc>, Error>
    where
        T: serde::Serialize,
    {
        value.serialize(self)
    }

    fn serialize_unit(self) -> Result<Value<'gc>, Error> {
        Ok(unit(self.ctx).into())
    }

    fn serialize_unit_struct(self, _name: &'static str) -> Result<Value<'gc>, Error> {
        self.serialize_unit()
    }

    fn serialize_unit_variant(
        self,
        _name: &'static str,
        _variant_index: u32,
        variant: &'static str,
    ) -> Result<Value<'gc>, Error> {
        self.serialize_str(variant)
    }

    fn serialize_newtype_struct<T: ?Sized>(
        self,
        _name: &'static str,
        value: &T,
    ) -> Result<Value<'gc>, Error>
    where
        T: serde::Serialize,
    {
        value.serialize(self)
    }

    fn serialize_newtype_variant<T: ?Sized>(
        self,
        _name: &'static str,
        _variant_index: u32,
        variant: &'static str,
        value: &T,
    ) -> Result<Value<'gc>, Error>
    where
        T: serde::Serialize,
    {
        let value = value.serialize(self)?;
        let table = Table::new(&self.ctx);
        table.set_field(self.ctx, variant, value);
        Ok(table.into())
    }

    fn serialize_seq(self, _len: Option<usize>) -> Result<SerializeSeq<'gc>, Error> {
        Ok(SerializeSeq::new(self.ctx, self.options))
    }

    fn serialize_tuple(self, len: usize) -> Result<SerializeSeq<'gc>, Error> {
        self.serialize_seq(Some(len))
    }

    fn serialize_tuple_struct(
        self,
        _name: &'static str,
        len: usize,
    ) -> Result<SerializeSeq<'gc>, Error> {
        self.serialize_seq(Some(len))
    }

    fn serialize_tuple_variant(
        self,
        _name: &'static str,
        _variant_index: u32,
        variant: &'static str,
        _len: usize,
    ) -> Result<Self::SerializeTupleVariant, Error> {
        Ok(SerializeTupleVariant::new(self.ctx, self.options, variant))
    }

    fn serialize_map(self, _len: Option<usize>) -> Result<Self::SerializeMap, Error> {
        Ok(SerializeMap::new(self.ctx, self.options))
    }

    fn serialize_struct(
        self,
        _name: &'static str,
        _len: usize,
    ) -> Result<Self::SerializeStruct, Error> {
        Ok(SerializeStruct::new(self.ctx, self.options))
    }

    fn serialize_struct_variant(
        self,
        _name: &'static str,
        _variant_index: u32,
        variant: &'static str,
        _len: usize,
    ) -> Result<Self::SerializeStructVariant, Error> {
        Ok(SerializeStructVariant::new(self.ctx, self.options, variant))
    }
}

pub struct SerializeSeq<'gc> {
    ctx: Context<'gc>,
    options: Options,
    table: Table<'gc>,
    ind: i64,
}

impl<'gc> SerializeSeq<'gc> {
    pub fn new(ctx: Context<'gc>, options: Options) -> Self {
        Self {
            ctx,
            options,
            table: Table::new(&ctx),
            ind: 1,
        }
    }
}

impl<'gc> ser::SerializeSeq for SerializeSeq<'gc> {
    type Ok = Value<'gc>;
    type Error = Error;

    fn serialize_element<T: ?Sized>(&mut self, value: &T) -> Result<(), Error>
    where
        T: serde::Serialize,
    {
        self.table
            .set(
                self.ctx,
                self.ind,
                value.serialize(Serializer::new(self.ctx, self.options))?,
            )
            .unwrap();
        self.ind = self
            .ind
            .checked_add(1)
            .ok_or(ser::Error::custom("index overflow"))?;
        Ok(())
    }

    fn end(self) -> Result<Value<'gc>, Error> {
        Ok(self.table.into())
    }
}

impl<'gc> ser::SerializeTuple for SerializeSeq<'gc> {
    type Ok = Value<'gc>;
    type Error = Error;

    fn serialize_element<T: ?Sized>(&mut self, value: &T) -> Result<(), Error>
    where
        T: serde::Serialize,
    {
        ser::SerializeSeq::serialize_element(self, value)
    }

    fn end(self) -> Result<Value<'gc>, Error> {
        ser::SerializeSeq::end(self)
    }
}

impl<'gc> ser::SerializeTupleStruct for SerializeSeq<'gc> {
    type Ok = Value<'gc>;
    type Error = Error;

    fn serialize_field<T: ?Sized>(&mut self, value: &T) -> Result<(), Error>
    where
        T: serde::Serialize,
    {
        ser::SerializeSeq::serialize_element(self, value)
    }

    fn end(self) -> Result<Value<'gc>, Error> {
        ser::SerializeSeq::end(self)
    }
}

pub struct SerializeMap<'gc> {
    ctx: Context<'gc>,
    options: Options,
    table: Table<'gc>,
    next_key: Value<'gc>,
}

impl<'gc> SerializeMap<'gc> {
    pub fn new(ctx: Context<'gc>, options: Options) -> Self {
        Self {
            ctx,
            options,
            table: Table::new(&ctx),
            next_key: Value::Nil,
        }
    }
}

impl<'gc> ser::SerializeMap for SerializeMap<'gc> {
    type Ok = Value<'gc>;
    type Error = Error;

    fn serialize_key<T: ?Sized>(&mut self, key: &T) -> Result<(), Self::Error>
    where
        T: serde::Serialize,
    {
        self.next_key = key.serialize(Serializer::new(self.ctx, self.options))?;
        Ok(())
    }

    fn serialize_value<T: ?Sized>(&mut self, value: &T) -> Result<(), Self::Error>
    where
        T: serde::Serialize,
    {
        self.table
            .set(
                self.ctx,
                self.next_key,
                value.serialize(Serializer::new(self.ctx, self.options))?,
            )
            .map_err(|_| {
                ser::Error::custom("key in map / struct must not serialize to Nil / NaN")
            })?;
        self.next_key = Value::Nil;
        Ok(())
    }

    fn end(self) -> Result<Self::Ok, Self::Error> {
        Ok(self.table.into())
    }
}

pub struct SerializeStruct<'gc> {
    ctx: Context<'gc>,
    options: Options,
    table: Table<'gc>,
}

impl<'gc> SerializeStruct<'gc> {
    pub fn new(ctx: Context<'gc>, options: Options) -> Self {
        Self {
            ctx,
            options,
            table: Table::new(&ctx),
        }
    }
}

impl<'gc> ser::SerializeStruct for SerializeStruct<'gc> {
    type Ok = Value<'gc>;
    type Error = Error;

    fn serialize_field<T: ?Sized>(
        &mut self,
        key: &'static str,
        value: &T,
    ) -> Result<(), Self::Error>
    where
        T: serde::Serialize,
    {
        self.table.set_field(
            self.ctx,
            key,
            value.serialize(Serializer::new(self.ctx, self.options))?,
        );
        Ok(())
    }

    fn end(self) -> Result<Self::Ok, Self::Error> {
        Ok(self.table.into())
    }
}

pub struct SerializeTupleVariant<'gc> {
    ctx: Context<'gc>,
    options: Options,
    variant: &'static str,
    table: Table<'gc>,
    ind: i64,
}

impl<'gc> SerializeTupleVariant<'gc> {
    pub fn new(ctx: Context<'gc>, options: Options, variant: &'static str) -> Self {
        Self {
            ctx,
            options,
            variant,
            table: Table::new(&ctx),
            ind: 1,
        }
    }
}

impl<'gc> ser::SerializeTupleVariant for SerializeTupleVariant<'gc> {
    type Ok = Value<'gc>;
    type Error = Error;

    fn serialize_field<T: ?Sized>(&mut self, value: &T) -> Result<(), Self::Error>
    where
        T: serde::Serialize,
    {
        self.table
            .set(
                self.ctx,
                self.ind,
                value.serialize(Serializer::new(self.ctx, self.options))?,
            )
            .unwrap();
        self.ind = self
            .ind
            .checked_add(1)
            .ok_or(ser::Error::custom("index overflow"))?;
        Ok(())
    }

    fn end(self) -> Result<Self::Ok, Self::Error> {
        let enclosing = Table::new(&self.ctx);
        enclosing.set_field(self.ctx, self.variant, self.table);
        Ok(enclosing.into())
    }
}

pub struct SerializeStructVariant<'gc> {
    ctx: Context<'gc>,
    options: Options,
    variant: &'static str,
    table: Table<'gc>,
}

impl<'gc> SerializeStructVariant<'gc> {
    pub fn new(ctx: Context<'gc>, options: Options, variant: &'static str) -> Self {
        Self {
            ctx,
            options,
            variant,
            table: Table::new(&ctx),
        }
    }
}

impl<'gc> ser::SerializeStructVariant for SerializeStructVariant<'gc> {
    type Ok = Value<'gc>;
    type Error = Error;

    fn serialize_field<T: ?Sized>(
        &mut self,
        key: &'static str,
        value: &T,
    ) -> Result<(), Self::Error>
    where
        T: serde::Serialize,
    {
        self.table.set_field(
            self.ctx,
            key,
            value.serialize(Serializer::new(self.ctx, self.options))?,
        );
        Ok(())
    }

    fn end(self) -> Result<Self::Ok, Self::Error> {
        let enclosing = Table::new(&self.ctx);
        enclosing.set_field(self.ctx, self.variant, self.table);
        Ok(enclosing.into())
    }
}

/// A `Value` paired with the context needed to read it, so it can be serialized into any serde
/// format.
///
/// This is the direction `to_value` does not cover: `to_value` builds a Lua value from Rust data,
/// while this takes a Lua value and writes it out as JSON, TOML, or anything else with a serde
/// backend. It is what makes a value crossing a thread boundary practical — serialize on the
/// worker, send owned bytes, deserialize on the other side.
///
/// A bare `impl Serialize for Value` is not possible: reading a table needs the arena context, and
/// a weak-valued table needs it to know which entries are still live. Pairing the two is the honest
/// spelling.
///
/// ```
/// # use luna::{Lua, Table, Context};
/// # use luna_util::serde::SerializeValue;
/// # let mut lua = Lua::core();
/// lua.enter(|ctx| {
///     let table = Table::new(&ctx);
///     table.set(ctx, "name", "luna").unwrap();
///     let json = serde_json::to_string(&SerializeValue::new(ctx, table.into())).unwrap();
///     assert_eq!(json, r#"{"name":"luna"}"#);
/// });
/// ```
#[derive(Copy, Clone)]
pub struct SerializeValue<'gc> {
    ctx: Context<'gc>,
    value: Value<'gc>,
    depth: usize,
}

/// Cyclic tables have no serde representation, and the recursion is not bounded by the input's
/// size. Matches the deserializer's limit.
const MAX_DEPTH: usize = 128;

impl<'gc> SerializeValue<'gc> {
    pub fn new(ctx: Context<'gc>, value: Value<'gc>) -> Self {
        Self {
            ctx,
            value,
            depth: 0,
        }
    }

    fn nested(self, value: Value<'gc>) -> Self {
        Self {
            ctx: self.ctx,
            value,
            depth: self.depth + 1,
        }
    }
}

/// Whether a table is a `1..=n` sequence, and so serializes as an array rather than a map.
fn sequence_length<'gc>(ctx: Context<'gc>, table: Table<'gc>) -> Option<usize> {
    let length = table.length(&ctx);
    if length < 0 {
        return None;
    }
    let mut counted = 0usize;
    for (key, _) in table.iter(ctx) {
        match key.to_integer() {
            Some(i) if i >= 1 && i <= length => counted += 1,
            _ => return None,
        }
    }
    (counted == length as usize).then_some(counted)
}

impl<'gc> ser::Serialize for SerializeValue<'gc> {
    fn serialize<S: ser::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use ser::{Error as _, SerializeMap, SerializeSeq};

        if self.depth > MAX_DEPTH {
            return Err(S::Error::custom(
                "table nests deeper than 128 levels, or is cyclic",
            ));
        }

        match self.value {
            Value::Nil => serializer.serialize_unit(),
            Value::Boolean(b) => serializer.serialize_bool(b),
            Value::Integer(i) => serializer.serialize_i64(i),
            Value::Number(n) => serializer.serialize_f64(n),
            Value::String(s) => match s.to_str() {
                Ok(s) => serializer.serialize_str(s),
                Err(_) => serializer.serialize_bytes(s.as_bytes()),
            },
            Value::Table(t) => match sequence_length(self.ctx, t) {
                Some(length) => {
                    let mut seq = serializer.serialize_seq(Some(length))?;
                    for i in 1..=length {
                        let element = t.get_value(self.ctx, Value::Integer(i as i64));
                        seq.serialize_element(&self.nested(element))?;
                    }
                    seq.end()
                }
                None => {
                    let mut map = serializer.serialize_map(None)?;
                    for (key, value) in t.iter(self.ctx) {
                        map.serialize_entry(&self.nested(key), &self.nested(value))?;
                    }
                    map.end()
                }
            },
            other => Err(S::Error::custom(format!(
                "cannot serialize a {}",
                other.type_name()
            ))),
        }
    }
}
