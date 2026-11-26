//! Write-only declaration logs, so registration-style configuration stays pure.
//!
//! A host that would normally act on `oslo.env.set("X", "1")` immediately instead describes its
//! API here. Calls are appended to a log the script cannot read, with their arguments frozen, and
//! the host applies the log after evaluation. Reads of host state become fixed inputs.

use ottavino_gc_arena::Collect;

use crate::{Callback, CallbackReturn, Context, Table, Value};

use super::{freeze, Data, DataError};

#[derive(Debug, Clone)]
enum Entry {
    Record,
    Returning(Data),
    Value(Data),
}

/// The shape of a registration-style API: dotted paths to recorded functions, functions that
/// return a fixed value, and plain values.
#[derive(Debug, Clone, Default)]
pub struct Api {
    entries: Vec<(String, Entry)>,
}

impl Api {
    pub fn new() -> Self {
        Self::default()
    }

    /// A function at `path` whose calls are recorded and which returns nothing.
    pub fn record(mut self, path: &str) -> Self {
        self.entries.push((path.to_owned(), Entry::Record));
        self
    }

    /// A function at `path` that always returns `value`, for host state read as an input.
    pub fn returning(mut self, path: &str, value: impl Into<Data>) -> Self {
        self.entries
            .push((path.to_owned(), Entry::Returning(value.into())));
        self
    }

    /// A plain value at `path`.
    pub fn value(mut self, path: &str, value: impl Into<Data>) -> Self {
        self.entries
            .push((path.to_owned(), Entry::Value(value.into())));
        self
    }

    /// The canonical description of this API, for content hashing.
    pub fn describe(&self) -> Data {
        let mut entries: Vec<Data> = self
            .entries
            .iter()
            .map(|(path, entry)| {
                let (kind, value) = match entry {
                    Entry::Record => ("record", Data::Nil),
                    Entry::Returning(value) => ("returning", value.clone()),
                    Entry::Value(value) => ("value", value.clone()),
                };
                Data::List(vec![Data::from(path.as_str()), Data::from(kind), value])
            })
            .collect();
        entries.sort_by_key(|entry| entry.canonical());
        Data::List(entries)
    }

    /// Build the frozen API table and the log its recorded functions append to.
    pub fn install<'gc>(
        &self,
        ctx: Context<'gc>,
    ) -> Result<(Table<'gc>, Recorder<'gc>), DataError> {
        let root = Table::new(&ctx);
        let recorder = Recorder(Table::new(&ctx));
        for (path, entry) in &self.entries {
            let mut table = root;
            let mut parts = path.split('.').peekable();
            while let Some(part) = parts.next() {
                if part.is_empty() {
                    return Err(DataError {
                        path: path.clone(),
                        message: "an API path cannot have an empty part".to_owned(),
                    });
                }
                let key = Value::String(ctx.intern(part.as_bytes()));
                if parts.peek().is_none() {
                    if !table.get_raw(&ctx, key).is_nil() {
                        return Err(DataError {
                            path: path.clone(),
                            message: "this API path is defined twice".to_owned(),
                        });
                    }
                    let value = match entry {
                        Entry::Record => recorded(ctx, recorder, path),
                        Entry::Returning(data) => returning(ctx, data.to_value(ctx)?),
                        Entry::Value(data) => data.to_value(ctx)?,
                    };
                    table.set_raw(&ctx, key, value).unwrap();
                } else {
                    table = match table.get_raw(&ctx, key) {
                        Value::Table(next) => next,
                        Value::Nil => {
                            let next = Table::new(&ctx);
                            table.set_raw(&ctx, key, next.into()).unwrap();
                            next
                        }
                        _ => {
                            return Err(DataError {
                                path: path.clone(),
                                message: "this API path runs through a value".to_owned(),
                            })
                        }
                    };
                }
            }
        }
        freeze(ctx, root.into());
        Ok((root, recorder))
    }
}

/// The write-only log behind an installed [`Api`].
#[derive(Debug, Copy, Clone, Collect)]
#[collect(no_drop)]
pub struct Recorder<'gc>(Table<'gc>);

/// One recorded call: the API path and its frozen arguments.
#[derive(Debug, Clone)]
pub struct Declaration<'gc> {
    pub name: String,
    pub args: Vec<Value<'gc>>,
}

impl<'gc> Recorder<'gc> {
    /// Rebuild a recorder from the table returned by [`Recorder::table`], for example after
    /// stashing it between `enter` calls.
    pub fn from_table(table: Table<'gc>) -> Self {
        Self(table)
    }

    /// The log table, for stashing. It is never reachable from Lua.
    pub fn table(self) -> Table<'gc> {
        self.0
    }

    /// Every recorded call, in the order the script made them.
    pub fn declarations(self, ctx: Context<'gc>) -> Vec<Declaration<'gc>> {
        let mut declarations = Vec::new();
        for index in 1..=self.0.length(&ctx) {
            let Value::Table(entry) = self.0.get_raw(&ctx, Value::Integer(index)) else {
                continue;
            };
            let name = match entry.get_raw(&ctx, Value::Integer(1)) {
                Value::String(name) => name.display_lossy().to_string(),
                _ => continue,
            };
            let count = match entry.get_raw(&ctx, Value::Integer(2)) {
                Value::Integer(count) => count,
                _ => 0,
            };
            let args = (0..count)
                .map(|i| entry.get_raw(&ctx, Value::Integer(i + 3)))
                .collect();
            declarations.push(Declaration { name, args });
        }
        declarations
    }

    /// Every recorded call as data: a list of `{ name, args }` maps.
    pub fn to_data(self, ctx: Context<'gc>) -> Result<Data, DataError> {
        let mut out = Vec::new();
        for (index, declaration) in self.declarations(ctx).into_iter().enumerate() {
            let mut args = Vec::new();
            for (position, arg) in declaration.args.into_iter().enumerate() {
                args.push(Data::from_value(ctx, arg).map_err(|error| DataError {
                    path: format!(
                        "declaration {} ({}) argument {}{}",
                        index + 1,
                        declaration.name,
                        position + 1,
                        error.path
                    ),
                    message: error.message,
                })?);
            }
            out.push(Data::map(vec![
                (Data::from("args"), Data::List(args)),
                (Data::from("name"), Data::from(declaration.name)),
            ])?);
        }
        Ok(Data::List(out))
    }
}

#[derive(Collect)]
#[collect(no_drop)]
struct Recording<'gc> {
    log: Table<'gc>,
    name: Value<'gc>,
}

fn recorded<'gc>(ctx: Context<'gc>, recorder: Recorder<'gc>, path: &str) -> Value<'gc> {
    let root = Recording {
        log: recorder.0,
        name: Value::String(ctx.intern(path.as_bytes())),
    };
    Callback::from_fn_with(&ctx, root, |root, ctx, _, mut stack| {
        let entry = Table::new(&ctx);
        entry.set_raw(&ctx, Value::Integer(1), root.name).unwrap();
        entry
            .set_raw(&ctx, Value::Integer(2), Value::Integer(stack.len() as i64))
            .unwrap();
        for (i, arg) in stack.drain(..).enumerate() {
            freeze(ctx, arg);
            entry
                .set_raw(&ctx, Value::Integer(i as i64 + 3), arg)
                .unwrap();
        }
        let next = root.log.length(&ctx) + 1;
        root.log.set_raw(&ctx, Value::Integer(next), entry.into())?;
        Ok(CallbackReturn::Return)
    })
    .into()
}

fn returning<'gc>(ctx: Context<'gc>, value: Value<'gc>) -> Value<'gc> {
    freeze(ctx, value);
    Callback::from_fn_with(&ctx, value, |value, ctx, _, mut stack| {
        stack.replace(ctx, *value);
        Ok(CallbackReturn::Return)
    })
    .into()
}
