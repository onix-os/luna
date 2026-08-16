//! Derive macros for [`luna`](https://docs.rs/luna)'s conversion traits.
//!
//! A separate crate, and behind luna's `derive` feature, because deriving costs three build
//! dependencies — `syn`, `quote` and `proc-macro2` — that a host writing its conversions by hand
//! should not have to compile.
//!
//! ```ignore
//! #[derive(FromValue, IntoValue)]
//! struct Point { x: i64, y: i64 }
//! ```
//!
//! A struct becomes a table keyed by field name; a fieldless enum becomes the variant's name as a
//! string. Anything else is rejected at compile time rather than guessed at.

use proc_macro::TokenStream;
use quote::quote;
use syn::{parse_quote, spanned::Spanned, Data, DeriveInput, Fields, GenericParam, Generics};

/// Builds the impl's generics from the type's own: `'gc` is prepended, and every type parameter
/// gains `bound` — a field of type `T` can only be converted if `T` converts too.
///
/// `'gc` cannot simply be written into the `impl<...>` list beside the split-off generics, because
/// `impl_generics` renders its own angle brackets and `impl<'gc, <T>>` does not parse.
fn impl_generics(generics: &Generics, bound: &syn::TypeParamBound) -> Generics {
    let mut generics = generics.clone();
    for param in &mut generics.params {
        if let GenericParam::Type(ty) = param {
            ty.bounds.push(bound.clone());
        }
    }
    generics.params.insert(0, parse_quote!('gc));
    generics
}

/// Derive [`IntoValue`](https://docs.rs/luna/latest/luna/trait.IntoValue.html): a struct becomes a
/// table keyed by field name, a fieldless enum becomes its variant name.
#[proc_macro_derive(IntoValue)]
pub fn derive_into_value(input: TokenStream) -> TokenStream {
    let input = syn::parse_macro_input!(input as DeriveInput);
    match into_value_body(&input) {
        Ok(body) => {
            let name = &input.ident;
            let generics = impl_generics(&input.generics, &parse_quote!(luna::IntoValue<'gc>));
            let (impl_generics, _, where_clause) = generics.split_for_impl();
            let (_, ty_generics, _) = input.generics.split_for_impl();
            quote! {
                impl #impl_generics luna::IntoValue<'gc> for #name #ty_generics
                #where_clause
                {
                    fn into_value(self, ctx: luna::Context<'gc>) -> luna::Value<'gc> {
                        #body
                    }
                }
            }
            .into()
        }
        Err(err) => err.to_compile_error().into(),
    }
}

/// Derive [`FromValue`](https://docs.rs/luna/latest/luna/trait.FromValue.html), the inverse of
/// [`macro@IntoValue`].
#[proc_macro_derive(FromValue)]
pub fn derive_from_value(input: TokenStream) -> TokenStream {
    let input = syn::parse_macro_input!(input as DeriveInput);
    match from_value_body(&input) {
        Ok(body) => {
            let name = &input.ident;
            let generics = impl_generics(&input.generics, &parse_quote!(luna::FromValue<'gc>));
            let (impl_generics, _, where_clause) = generics.split_for_impl();
            let (_, ty_generics, _) = input.generics.split_for_impl();
            quote! {
                impl #impl_generics luna::FromValue<'gc> for #name #ty_generics
                #where_clause
                {
                    fn from_value(
                        ctx: luna::Context<'gc>,
                        value: luna::Value<'gc>,
                    ) -> Result<Self, luna::TypeError> {
                        #body
                    }
                }
            }
            .into()
        }
        Err(err) => err.to_compile_error().into(),
    }
}

fn into_value_body(input: &DeriveInput) -> syn::Result<proc_macro2::TokenStream> {
    match &input.data {
        Data::Struct(data) => {
            let fields = named_fields(&data.fields, input)?;
            let sets = fields.iter().map(|field| {
                let ident = field.ident.as_ref().expect("named");
                let key = ident.to_string();
                quote! { table.set_field(ctx, #key, self.#ident); }
            });
            Ok(quote! {
                let table = luna::Table::new(&ctx);
                #(#sets)*
                luna::Value::Table(table)
            })
        }
        Data::Enum(data) => {
            let arms = data
                .variants
                .iter()
                .map(|variant| {
                    if !matches!(variant.fields, Fields::Unit) {
                        return Err(syn::Error::new(
                            variant.span(),
                            "IntoValue derives only fieldless enum variants; \
                             write the impl by hand for a variant with data",
                        ));
                    }
                    let ident = &variant.ident;
                    let name = ident.to_string();
                    Ok(quote! { Self::#ident => luna::IntoValue::into_value(#name, ctx), })
                })
                .collect::<syn::Result<Vec<_>>>()?;
            Ok(quote! {
                match self {
                    #(#arms)*
                }
            })
        }
        Data::Union(_) => Err(syn::Error::new(
            input.span(),
            "IntoValue cannot be derived for a union",
        )),
    }
}

fn from_value_body(input: &DeriveInput) -> syn::Result<proc_macro2::TokenStream> {
    match &input.data {
        Data::Struct(data) => {
            let fields = named_fields(&data.fields, input)?;
            let gets = fields.iter().map(|field| {
                let ident = field.ident.as_ref().expect("named");
                let key = ident.to_string();
                quote! { #ident: table.get(ctx, #key)?, }
            });
            Ok(quote! {
                let table: luna::Table<'gc> = luna::FromValue::from_value(ctx, value)?;
                Ok(Self { #(#gets)* })
            })
        }
        Data::Enum(data) => {
            let arms = data
                .variants
                .iter()
                .map(|variant| {
                    if !matches!(variant.fields, Fields::Unit) {
                        return Err(syn::Error::new(
                            variant.span(),
                            "FromValue derives only fieldless enum variants; \
                             write the impl by hand for a variant with data",
                        ));
                    }
                    let ident = &variant.ident;
                    let name = ident.to_string();
                    Ok(quote! { #name => Ok(Self::#ident), })
                })
                .collect::<syn::Result<Vec<_>>>()?;
            let expected = format!("one of the {} variants", input.ident);
            Ok(quote! {
                let name: luna::String<'gc> = luna::FromValue::from_value(ctx, value)?;
                match name.to_str().map_err(|_| luna::TypeError {
                    expected: #expected,
                    found: "a non-utf8 string",
                })? {
                    #(#arms)*
                    _ => Err(luna::TypeError {
                        expected: #expected,
                        found: "an unknown variant",
                    }),
                }
            })
        }
        Data::Union(_) => Err(syn::Error::new(
            input.span(),
            "FromValue cannot be derived for a union",
        )),
    }
}

/// Only named fields make sense as table keys; a tuple struct has nothing to call them.
fn named_fields<'a>(fields: &'a Fields, input: &DeriveInput) -> syn::Result<Vec<&'a syn::Field>> {
    match fields {
        Fields::Named(named) => Ok(named.named.iter().collect()),
        Fields::Unit => Ok(Vec::new()),
        Fields::Unnamed(_) => Err(syn::Error::new(
            input.span(),
            "a tuple struct has no field names to use as table keys; \
             give the fields names or write the impl by hand",
        )),
    }
}
