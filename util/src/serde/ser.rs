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
pub struct Options {
    /// If true, serialize the special `none` marker instead of `nil`.
    pub serialize_none: bool,
    /// If true, serialize the special `unit` marker instead of `nil`.
    ///
    /// Defaults to true, which is what this always did. The markers exist so that a round trip
    /// can tell `None` and `()` apart from a genuine `nil`; turn this off when the Lua side would
    /// rather see a plain `nil` than a userdata it has no use for.
    pub serialize_unit: bool,
}

impl Default for Options {
    fn default() -> Self {
        Options {
            serialize_none: false,
            serialize_unit: true,
        }
    }
}

impl Options {
    pub fn serialize_none(mut self, enabled: bool) -> Self {
        self.serialize_none = enabled;
        self
    }

    pub fn serialize_unit(mut self, enabled: bool) -> Self {
        self.serialize_unit = enabled;
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
        Ok(if self.options.serialize_unit {
            unit(self.ctx).into()
        } else {
            Value::Nil
        })
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
/// How a Lua value is written out by [`SerializeValue`].
#[derive(Debug, Copy, Clone)]
#[non_exhaustive]
pub struct ValueOptions {
    /// Emit map keys in sorted order rather than the table's own iteration order.
    ///
    /// luna's tables iterate in insertion order, which is already stable for a table built the same
    /// way twice. This is for the stronger property: the same *set* of entries produces the same
    /// bytes however it was assembled — what a content hash or a golden file needs.
    pub sort_keys: bool,
    /// If false, a function, thread or userdata is skipped instead of failing the whole document.
    ///
    /// Defaults to true. Skipping is what you want when serializing a config table that happens to
    /// carry a few Lua-side helpers; failing is what you want everywhere else, which is why that
    /// is the default.
    pub deny_unsupported_types: bool,
    /// How deep nesting may go before serialization gives up.
    ///
    /// A cyclic table has no serde representation and the recursion is not bounded by the input's
    /// size, so this is a guard against a crash rather than a tuning knob.
    pub max_depth: usize,
}

impl Default for ValueOptions {
    fn default() -> Self {
        ValueOptions {
            sort_keys: false,
            deny_unsupported_types: true,
            max_depth: MAX_DEPTH,
        }
    }
}

impl ValueOptions {
    pub fn sort_keys(mut self, enabled: bool) -> Self {
        self.sort_keys = enabled;
        self
    }

    pub fn deny_unsupported_types(mut self, enabled: bool) -> Self {
        self.deny_unsupported_types = enabled;
        self
    }

    pub fn max_depth(mut self, depth: usize) -> Self {
        self.max_depth = depth;
        self
    }
}

#[derive(Copy, Clone)]
pub struct SerializeValue<'gc> {
    ctx: Context<'gc>,
    value: Value<'gc>,
    options: ValueOptions,
    depth: usize,
}

/// Cyclic tables have no serde representation, and the recursion is not bounded by the input's
/// size. Matches the deserializer's limit.
const MAX_DEPTH: usize = 128;

impl<'gc> SerializeValue<'gc> {
    pub fn new(ctx: Context<'gc>, value: Value<'gc>) -> Self {
        Self::with_options(ctx, value, ValueOptions::default())
    }

    pub fn with_options(ctx: Context<'gc>, value: Value<'gc>, options: ValueOptions) -> Self {
        Self {
            ctx,
            value,
            options,
            depth: 0,
        }
    }

    fn nested(self, value: Value<'gc>) -> Self {
        Self {
            ctx: self.ctx,
            value,
            options: self.options,
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

        if self.depth > self.options.max_depth {
            return Err(S::Error::custom(format!(
                "table nests deeper than {} levels, or is cyclic",
                self.options.max_depth
            )));
        }

        /// Whether a value has a serde representation at all.
        fn representable(value: Value<'_>) -> bool {
            !matches!(
                value,
                Value::Function(_) | Value::Thread(_) | Value::UserData(_)
            )
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
                    let mut entries: std::vec::Vec<_> = t
                        .iter(self.ctx)
                        .filter(|(key, value)| {
                            self.options.deny_unsupported_types
                                || (representable(*key) && representable(*value))
                        })
                        .collect();

                    if self.options.sort_keys {
                        // `Value` has no ordering across types, so keys are grouped by kind first
                        // and compared within a kind — numbers numerically, strings by bytes. Any
                        // total order would do; this one is the least surprising to read.
                        fn kind(value: &Value<'_>) -> u8 {
                            match value {
                                Value::Integer(_) | Value::Number(_) => 0,
                                Value::String(_) => 1,
                                Value::Boolean(_) => 2,
                                _ => 3,
                            }
                        }

                        entries.sort_by(|(a, _), (b, _)| {
                            kind(a).cmp(&kind(b)).then_with(|| match (a, b) {
                                (Value::String(a), Value::String(b)) => {
                                    a.as_bytes().cmp(b.as_bytes())
                                }
                                (Value::Boolean(a), Value::Boolean(b)) => a.cmp(b),
                                _ => match (a.to_number(), b.to_number()) {
                                    (Some(a), Some(b)) => {
                                        a.partial_cmp(&b).unwrap_or(std::cmp::Ordering::Equal)
                                    }
                                    _ => std::cmp::Ordering::Equal,
                                },
                            })
                        });
                    }

                    let mut map = serializer.serialize_map(Some(entries.len()))?;
                    for (key, value) in entries {
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
