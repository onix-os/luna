use std::fmt;

use luna::{table::NextValue, Context, Table, Value};
use serde::de;
use thiserror::Error;

use super::markers::{is_none, is_unit};

#[derive(Debug, Error)]
pub enum Error {
    #[error("{0}")]
    Message(String),
    #[error("expected {expected}, found {found}")]
    TypeError {
        expected: &'static str,
        found: &'static str,
    },
}

impl de::Error for Error {
    fn custom<T: fmt::Display>(msg: T) -> Self {
        Error::Message(msg.to_string())
    }
}

/// How deeply a Lua value may nest before deserialization gives up.
///
/// Without a bound, `local t = {} t.self = t` recurses until the *process* dies — a crash, not an
/// error a host can catch. The limit is generous enough that no honest document reaches it.
const MAX_DEPTH: usize = 128;

thread_local! {
    /// Nesting depth of the deserialization in progress.
    ///
    /// Held here rather than threaded through every `Deserializer`, `SeqAccess` and `MapAccess`
    /// because they are constructed in a dozen places and none of them otherwise care. Restored by
    /// `Drop`, so an early return or a panic cannot leave it raised.
    static DEPTH: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

struct DepthGuard;

impl DepthGuard {
    fn enter() -> Result<Self, Error> {
        DEPTH.with(|d| {
            let limit = options().max_depth;
            if d.get() >= limit {
                Err(Error::Message(format!(
                    "value nests deeper than {limit} levels (is it cyclic?)"
                )))
            } else {
                d.set(d.get() + 1);
                Ok(DepthGuard)
            }
        })
    }
}

impl Drop for DepthGuard {
    fn drop(&mut self) {
        DEPTH.with(|d| d.set(d.get().saturating_sub(1)));
    }
}

/// How a Lua value is read back into Rust.
#[derive(Debug, Copy, Clone)]
#[non_exhaustive]
pub struct Options {
    /// How deeply a value may nest before deserialization gives up.
    ///
    /// The bound exists because `local t = {} t.self = t` would otherwise recurse until the
    /// *process* dies — a crash rather than an error a host can catch.
    pub max_depth: usize,
    /// If false, a function, thread or userdata deserializes as a unit instead of failing.
    ///
    /// Defaults to true. Turn it off to read a table that carries Lua-side helpers alongside the
    /// data you actually want.
    pub deny_unsupported_types: bool,
}

impl Default for Options {
    fn default() -> Self {
        Options {
            max_depth: MAX_DEPTH,
            deny_unsupported_types: true,
        }
    }
}

impl Options {
    pub fn max_depth(mut self, depth: usize) -> Self {
        self.max_depth = depth;
        self
    }

    pub fn deny_unsupported_types(mut self, enabled: bool) -> Self {
        self.deny_unsupported_types = enabled;
        self
    }
}

thread_local! {
    /// Options for the deserialization in progress.
    ///
    /// Alongside `DEPTH`, and for the reason given there: the `Deserializer`, `SeqAccess` and
    /// `MapAccess` types are constructed in a dozen places and none of them otherwise care. Set
    /// for the duration of a top-level call and restored afterwards, so a nested one cannot leak.
    static OPTIONS: std::cell::Cell<Options> = const {
        std::cell::Cell::new(Options {
            max_depth: MAX_DEPTH,
            deny_unsupported_types: true,
        })
    };
}

fn options() -> Options {
    OPTIONS.with(|o| o.get())
}

pub fn from_value<'gc, T: de::Deserialize<'gc>>(
    ctx: Context<'gc>,
    value: Value<'gc>,
) -> Result<T, Error> {
    from_value_with(ctx, value, Options::default())
}

pub fn from_value_with<'gc, T: de::Deserialize<'gc>>(
    ctx: Context<'gc>,
    value: Value<'gc>,
    options: Options,
) -> Result<T, Error> {
    /// Restores whatever was in effect, so a `from_value_with` reached from inside a `Deserialize`
    /// impl cannot change the options of the call that contains it.
    struct OptionsGuard(Options);

    impl Drop for OptionsGuard {
        fn drop(&mut self) {
            OPTIONS.with(|o| o.set(self.0));
        }
    }

    let _guard = OptionsGuard(OPTIONS.with(|o| o.get()));
    OPTIONS.with(|o| o.set(options));

    // A fresh top-level call starts from zero even if a previous one unwound oddly.
    DEPTH.with(|d| d.set(0));
    T::deserialize(Deserializer::from_value(ctx, value))
}

pub struct Deserializer<'gc> {
    ctx: Context<'gc>,
    value: Value<'gc>,
}

impl<'gc> Deserializer<'gc> {
    pub fn from_value(ctx: Context<'gc>, value: Value<'gc>) -> Self {
        Self { ctx, value }
    }
}

impl<'gc> de::Deserializer<'gc> for Deserializer<'gc> {
    type Error = Error;

    fn deserialize_any<V: de::Visitor<'gc>>(self, visitor: V) -> Result<V::Value, Error> {
        match self.value {
            Value::Nil => self.deserialize_unit(visitor),
            Value::Boolean(_) => self.deserialize_bool(visitor),
            Value::Integer(_) => self.deserialize_i64(visitor),
            Value::Number(_) => self.deserialize_f64(visitor),
            Value::String(s) => {
                if let Ok(string) = s.to_str() {
                    visitor.visit_borrowed_str(string)
                } else {
                    self.deserialize_bytes(visitor)
                }
            }
            Value::Table(t) => {
                if is_sequence(self.ctx, t) {
                    self.deserialize_seq(visitor)
                } else {
                    self.deserialize_map(visitor)
                }
            }
            Value::Function(_) | Value::Thread(_) if !options().deny_unsupported_types => {
                visitor.visit_unit()
            }
            Value::Function(_) => Err(de::Error::custom("cannot deserialize from function")),
            Value::Thread(_) => Err(de::Error::custom("cannot deserialize from thread")),
            Value::UserData(ud) => {
                if is_none(ud) {
                    self.deserialize_option(visitor)
                } else if is_unit(ud) {
                    self.deserialize_unit(visitor)
                } else if !options().deny_unsupported_types {
                    visitor.visit_unit()
                } else {
                    Err(de::Error::custom("cannot deserialize from userdata"))
                }
            }
        }
    }

    fn deserialize_bool<V>(self, visitor: V) -> Result<V::Value, Error>
    where
        V: de::Visitor<'gc>,
    {
        visitor.visit_bool(self.value.to_bool())
    }

    fn deserialize_i8<V>(self, visitor: V) -> Result<V::Value, Error>
    where
        V: de::Visitor<'gc>,
    {
        self.deserialize_i64(visitor)
    }

    fn deserialize_i16<V>(self, visitor: V) -> Result<V::Value, Error>
    where
        V: de::Visitor<'gc>,
    {
        self.deserialize_i64(visitor)
    }

    fn deserialize_i32<V>(self, visitor: V) -> Result<V::Value, Error>
    where
        V: de::Visitor<'gc>,
    {
        self.deserialize_i64(visitor)
    }

    fn deserialize_i64<V>(self, visitor: V) -> Result<V::Value, Error>
    where
        V: de::Visitor<'gc>,
    {
        if let Some(i) = self.value.to_integer() {
            visitor.visit_i64(i)
        } else {
            Err(Error::TypeError {
                expected: "integer",
                found: self.value.type_name(),
            })
        }
    }

    fn deserialize_u8<V>(self, visitor: V) -> Result<V::Value, Error>
    where
        V: de::Visitor<'gc>,
    {
        self.deserialize_i64(visitor)
    }

    fn deserialize_u16<V>(self, visitor: V) -> Result<V::Value, Error>
    where
        V: de::Visitor<'gc>,
    {
        self.deserialize_i64(visitor)
    }

    fn deserialize_u32<V>(self, visitor: V) -> Result<V::Value, Error>
    where
        V: de::Visitor<'gc>,
    {
        self.deserialize_i64(visitor)
    }

    fn deserialize_u64<V>(self, visitor: V) -> Result<V::Value, Error>
    where
        V: de::Visitor<'gc>,
    {
        self.deserialize_i64(visitor)
    }

    fn deserialize_f32<V>(self, visitor: V) -> Result<V::Value, Error>
    where
        V: de::Visitor<'gc>,
    {
        self.deserialize_f64(visitor)
    }

    fn deserialize_f64<V>(self, visitor: V) -> Result<V::Value, Error>
    where
        V: de::Visitor<'gc>,
    {
        if let Some(f) = self.value.to_number() {
            visitor.visit_f64(f)
        } else {
            Err(Error::TypeError {
                expected: "number",
                found: self.value.type_name(),
            })
        }
    }

    fn deserialize_char<V>(self, visitor: V) -> Result<V::Value, Error>
    where
        V: de::Visitor<'gc>,
    {
        self.deserialize_str(visitor)
    }

    fn deserialize_str<V>(self, visitor: V) -> Result<V::Value, Error>
    where
        V: de::Visitor<'gc>,
    {
        if let Value::String(s) = self.value {
            if let Ok(s) = s.to_str() {
                visitor.visit_borrowed_str(s)
            } else {
                Err(Error::TypeError {
                    expected: "utf8 string",
                    found: "non-utf8 string",
                })
            }
        } else if self.value.is_implicit_string() {
            visitor.visit_string(self.value.display().to_string())
        } else {
            Err(Error::TypeError {
                expected: "utf8 string",
                found: self.value.type_name(),
            })
        }
    }

    fn deserialize_string<V>(self, visitor: V) -> Result<V::Value, Error>
    where
        V: de::Visitor<'gc>,
    {
        self.deserialize_str(visitor)
    }

    fn deserialize_bytes<V>(self, visitor: V) -> Result<V::Value, Error>
    where
        V: de::Visitor<'gc>,
    {
        if let Value::String(s) = self.value {
            visitor.visit_borrowed_bytes(s.as_bytes())
        } else {
            Err(Error::TypeError {
                expected: "string",
                found: self.value.type_name(),
            })
        }
    }

    fn deserialize_byte_buf<V>(self, visitor: V) -> Result<V::Value, Error>
    where
        V: de::Visitor<'gc>,
    {
        self.deserialize_bytes(visitor)
    }

    fn deserialize_option<V>(self, visitor: V) -> Result<V::Value, Error>
    where
        V: de::Visitor<'gc>,
    {
        match self.value {
            Value::Nil => visitor.visit_none(),
            Value::UserData(ud) if is_none(ud) => visitor.visit_none(),
            _ => visitor.visit_some(self),
        }
    }

    fn deserialize_unit<V>(self, visitor: V) -> Result<V::Value, Error>
    where
        V: de::Visitor<'gc>,
    {
        match self.value {
            Value::Nil => visitor.visit_unit(),
            Value::UserData(ud) if is_unit(ud) => visitor.visit_unit(),
            v => Err(Error::TypeError {
                expected: "nil or unit",
                found: v.type_name(),
            }),
        }
    }

    fn deserialize_unit_struct<V>(self, _name: &'static str, visitor: V) -> Result<V::Value, Error>
    where
        V: de::Visitor<'gc>,
    {
        self.deserialize_unit(visitor)
    }

    fn deserialize_newtype_struct<V>(
        self,
        _name: &'static str,
        visitor: V,
    ) -> Result<V::Value, Error>
    where
        V: de::Visitor<'gc>,
    {
        visitor.visit_newtype_struct(self)
    }

    fn deserialize_seq<V>(self, visitor: V) -> Result<V::Value, Error>
    where
        V: de::Visitor<'gc>,
    {
        if let Value::Table(table) = self.value {
            // Held for the whole visit, so nesting is what is measured rather than total calls.
            let _guard = DepthGuard::enter()?;
            visitor.visit_seq(Seq::new(self.ctx, table))
        } else {
            Err(Error::TypeError {
                expected: "table",
                found: self.value.type_name(),
            })
        }
    }

    fn deserialize_tuple<V>(self, len: usize, visitor: V) -> Result<V::Value, Error>
    where
        V: de::Visitor<'gc>,
    {
        if let Value::Table(table) = self.value {
            visitor.visit_seq(Tuple::new(
                self.ctx,
                table,
                len.try_into()
                    .map_err(|_| de::Error::custom("tuple length out of range"))?,
            ))
        } else {
            Err(Error::TypeError {
                expected: "table",
                found: self.value.type_name(),
            })
        }
    }

    fn deserialize_tuple_struct<V>(
        self,
        _name: &'static str,
        len: usize,
        visitor: V,
    ) -> Result<V::Value, Error>
    where
        V: de::Visitor<'gc>,
    {
        self.deserialize_tuple(len, visitor)
    }

    fn deserialize_map<V>(self, visitor: V) -> Result<V::Value, Error>
    where
        V: de::Visitor<'gc>,
    {
        if let Value::Table(table) = self.value {
            let _guard = DepthGuard::enter()?;
            visitor.visit_map(Map::new(self.ctx, table))
        } else {
            Err(Error::TypeError {
                expected: "table",
                found: self.value.type_name(),
            })
        }
    }

    fn deserialize_struct<V>(
        self,
        _name: &'static str,
        _fields: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value, Error>
    where
        V: de::Visitor<'gc>,
    {
        self.deserialize_map(visitor)
    }

    fn deserialize_enum<V>(
        self,
        _name: &'static str,
        _variants: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value, Error>
    where
        V: de::Visitor<'gc>,
    {
        match self.value {
            Value::Table(table) => match table.next(&self.ctx, Value::Nil) {
                NextValue::Found { key, value } => {
                    visitor.visit_enum(Enum::new(self.ctx, key, value))
                }
                NextValue::Last => Err(de::Error::custom("enum table has no entries")),
                NextValue::NotFound => unreachable!(),
            },
            v => visitor.visit_enum(UnitEnum::new(self.ctx, v)),
        }
    }

    fn deserialize_identifier<V>(self, visitor: V) -> Result<V::Value, Error>
    where
        V: de::Visitor<'gc>,
    {
        self.deserialize_str(visitor)
    }

    fn deserialize_ignored_any<V>(self, visitor: V) -> Result<V::Value, Error>
    where
        V: de::Visitor<'gc>,
    {
        self.deserialize_any(visitor)
    }
}

pub struct Seq<'gc> {
    ctx: Context<'gc>,
    table: Table<'gc>,
    ind: i64,
}

impl<'gc> Seq<'gc> {
    fn new(ctx: Context<'gc>, table: Table<'gc>) -> Self {
        Self { ctx, table, ind: 1 }
    }
}

impl<'gc> de::SeqAccess<'gc> for Seq<'gc> {
    type Error = Error;

    fn next_element_seed<T>(&mut self, seed: T) -> Result<Option<T::Value>, Error>
    where
        T: de::DeserializeSeed<'gc>,
    {
        let v = self.table.get_raw(&self.ctx, Value::Integer(self.ind));
        if v.is_nil() {
            Ok(None)
        } else {
            let res = Some(seed.deserialize(Deserializer::from_value(self.ctx, v))?);
            self.ind = self
                .ind
                .checked_add(1)
                .ok_or(de::Error::custom("index overflow"))?;
            Ok(res)
        }
    }
}

pub struct Tuple<'gc> {
    ctx: Context<'gc>,
    table: Table<'gc>,
    len: i64,
    ind: i64,
}

impl<'gc> Tuple<'gc> {
    fn new(ctx: Context<'gc>, table: Table<'gc>, len: i64) -> Self {
        Self {
            ctx,
            table,
            len,
            ind: 1,
        }
    }
}

impl<'gc> de::SeqAccess<'gc> for Tuple<'gc> {
    type Error = Error;

    fn next_element_seed<T>(&mut self, seed: T) -> Result<Option<T::Value>, Error>
    where
        T: de::DeserializeSeed<'gc>,
    {
        if self.ind > self.len {
            Ok(None)
        } else {
            let v = self.table.get_raw(&self.ctx, Value::Integer(self.ind));
            let res = Some(seed.deserialize(Deserializer::from_value(self.ctx, v))?);
            self.ind += 1;
            Ok(res)
        }
    }
}

pub struct Map<'gc> {
    ctx: Context<'gc>,
    table: Table<'gc>,
    key: Value<'gc>,
    value: Value<'gc>,
}

impl<'gc> Map<'gc> {
    fn new(ctx: Context<'gc>, table: Table<'gc>) -> Self {
        Self {
            ctx,
            table,
            key: Value::Nil,
            value: Value::Nil,
        }
    }
}

impl<'gc> de::MapAccess<'gc> for Map<'gc> {
    type Error = Error;

    fn next_key_seed<K>(&mut self, seed: K) -> Result<Option<K::Value>, Error>
    where
        K: de::DeserializeSeed<'gc>,
    {
        match self.table.next(&self.ctx, self.key) {
            NextValue::Found { key, value } => {
                self.key = key;
                self.value = value;
                seed.deserialize(Deserializer::from_value(self.ctx, self.key))
                    .map(Some)
            }
            NextValue::Last => Ok(None),
            NextValue::NotFound => unreachable!(),
        }
    }

    fn next_value_seed<V>(&mut self, seed: V) -> Result<V::Value, Error>
    where
        V: de::DeserializeSeed<'gc>,
    {
        seed.deserialize(Deserializer::from_value(self.ctx, self.value))
    }
}

pub struct Enum<'gc> {
    ctx: Context<'gc>,
    key: Value<'gc>,
    value: Value<'gc>,
}

impl<'gc> Enum<'gc> {
    fn new(ctx: Context<'gc>, key: Value<'gc>, value: Value<'gc>) -> Self {
        Self { ctx, key, value }
    }
}

impl<'gc> de::EnumAccess<'gc> for Enum<'gc> {
    type Error = Error;
    type Variant = Variant<'gc>;

    fn variant_seed<V>(self, seed: V) -> Result<(V::Value, Variant<'gc>), Error>
    where
        V: de::DeserializeSeed<'gc>,
    {
        Ok((
            seed.deserialize(Deserializer::from_value(self.ctx, self.key))?,
            Variant::new(self.ctx, self.value),
        ))
    }
}

pub struct Variant<'gc> {
    ctx: Context<'gc>,
    value: Value<'gc>,
}

impl<'gc> Variant<'gc> {
    fn new(ctx: Context<'gc>, value: Value<'gc>) -> Self {
        Self { ctx, value }
    }
}

impl<'gc> de::VariantAccess<'gc> for Variant<'gc> {
    type Error = Error;

    fn unit_variant(self) -> Result<(), Error> {
        de::Deserialize::deserialize(Deserializer::from_value(self.ctx, self.value))
    }

    fn newtype_variant_seed<T>(self, seed: T) -> Result<T::Value, Error>
    where
        T: de::DeserializeSeed<'gc>,
    {
        seed.deserialize(Deserializer::from_value(self.ctx, self.value))
    }

    fn tuple_variant<V>(self, len: usize, visitor: V) -> Result<V::Value, Error>
    where
        V: de::Visitor<'gc>,
    {
        de::Deserializer::deserialize_tuple(
            Deserializer::from_value(self.ctx, self.value),
            len,
            visitor,
        )
    }

    fn struct_variant<V>(
        self,
        _fields: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value, Error>
    where
        V: de::Visitor<'gc>,
    {
        de::Deserializer::deserialize_map(Deserializer::from_value(self.ctx, self.value), visitor)
    }
}

pub struct UnitEnum<'gc> {
    ctx: Context<'gc>,
    key: Value<'gc>,
}

impl<'gc> UnitEnum<'gc> {
    fn new(ctx: Context<'gc>, key: Value<'gc>) -> Self {
        Self { ctx, key }
    }
}

impl<'gc> de::EnumAccess<'gc> for UnitEnum<'gc> {
    type Error = Error;
    type Variant = UnitVariant;

    fn variant_seed<V>(self, seed: V) -> Result<(V::Value, UnitVariant), Error>
    where
        V: de::DeserializeSeed<'gc>,
    {
        Ok((
            seed.deserialize(Deserializer::from_value(self.ctx, self.key))?,
            UnitVariant::new(),
        ))
    }
}

pub struct UnitVariant {}

impl UnitVariant {
    fn new() -> Self {
        Self {}
    }
}

impl<'de> de::VariantAccess<'de> for UnitVariant {
    type Error = Error;

    fn unit_variant(self) -> Result<(), Error> {
        Ok(())
    }

    fn newtype_variant_seed<T>(self, _seed: T) -> Result<T::Value, Error>
    where
        T: de::DeserializeSeed<'de>,
    {
        Err(Error::TypeError {
            expected: "table",
            found: "non-table",
        })
    }

    fn tuple_variant<V>(self, _len: usize, _visitor: V) -> Result<V::Value, Error>
    where
        V: de::Visitor<'de>,
    {
        Err(Error::TypeError {
            expected: "table",
            found: "non-table",
        })
    }

    fn struct_variant<V>(
        self,
        _fields: &'static [&'static str],
        _visitor: V,
    ) -> Result<V::Value, Error>
    where
        V: de::Visitor<'de>,
    {
        Err(Error::TypeError {
            expected: "table",
            found: "non-table",
        })
    }
}

fn is_sequence<'gc>(ctx: Context<'gc>, table: Table<'gc>) -> bool {
    let mut key = match table.next(&ctx, Value::Nil) {
        NextValue::Found { key, value: _ } => key,
        NextValue::Last => return true,
        NextValue::NotFound => unreachable!(),
    };

    let mut ind = 1;
    loop {
        if !matches!(key, Value::Integer(i) if i == ind) {
            return false;
        }

        ind = if let Some(i) = ind.checked_add(1) {
            i
        } else {
            return false;
        };

        key = match table.next(&ctx, key) {
            NextValue::Found { key, value: _ } => key,
            NextValue::Last => return true,
            NextValue::NotFound => unreachable!(),
        };
    }
}
