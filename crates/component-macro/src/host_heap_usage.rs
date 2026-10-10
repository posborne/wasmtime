use proc_macro2::TokenStream;
use quote::{format_ident, quote};
use syn::{Data, DeriveInput, Error, Fields, Result, parse_quote};

fn find_wasmtime_crate(attrs: &[syn::Attribute]) -> Result<syn::Path> {
    for attr in attrs {
        if !attr.path().is_ident("component") {
            continue;
        }
        let mut wasmtime_crate = None;
        attr.parse_nested_meta(|meta| {
            if meta.path.is_ident("wasmtime_crate") {
                let value = meta.value()?;
                wasmtime_crate = Some(value.parse()?);
                Ok(())
            } else {
                Ok(())
            }
        })?;
        if let Some(path) = wasmtime_crate {
            return Ok(path);
        }
    }
    Ok(parse_quote!(wasmtime))
}

pub fn expand_host_heap_usage(input: &DeriveInput) -> Result<TokenStream> {
    let wasmtime_crate = find_wasmtime_crate(&input.attrs)?;
    let ident = &input.ident;
    let (impl_generics, ty_generics, where_clause) = input.generics.split_for_impl();

    let mut where_clause = where_clause.cloned().unwrap_or_else(|| parse_quote!(where));
    for param in input.generics.type_params() {
        let param_ident = &param.ident;
        where_clause.predicates.push(parse_quote!(
            #param_ident: #wasmtime_crate::component::HostHeapUsage
        ));
    }

    let body = match &input.data {
        Data::Struct(data) => match &data.fields {
            Fields::Named(fields) => {
                let additions = fields.named.iter().map(|f| {
                    let name = &f.ident;
                    quote! { #wasmtime_crate::component::HostHeapUsage::heap_size_of_children(&self.#name) }
                });
                quote! {
                    0 #(+ #additions)*
                }
            }
            Fields::Unnamed(fields) => {
                let additions = (0..fields.unnamed.len()).map(|i| {
                    let index = syn::Index::from(i);
                    quote! { #wasmtime_crate::component::HostHeapUsage::heap_size_of_children(&self.#index) }
                });
                quote! {
                    0 #(+ #additions)*
                }
            }
            Fields::Unit => quote! { 0 },
        },
        Data::Enum(data) => {
            let arms = data.variants.iter().map(|variant| {
                let v_ident = &variant.ident;
                match &variant.fields {
                    Fields::Named(fields) => {
                        let names: Vec<_> = fields.named.iter().map(|f| &f.ident).collect();
                        let additions = names.iter().map(|name| {
                            quote! { #wasmtime_crate::component::HostHeapUsage::heap_size_of_children(#name) }
                        });
                        quote! {
                            Self::#v_ident { #(#names),* } => {
                                0 #(+ #additions)*
                            }
                        }
                    }
                    Fields::Unnamed(fields) => {
                        let names: Vec<_> = (0..fields.unnamed.len())
                            .map(|i| format_ident!("_f{}", i))
                            .collect();
                        let additions = names.iter().map(|name| {
                            quote! { #wasmtime_crate::component::HostHeapUsage::heap_size_of_children(#name) }
                        });
                        quote! {
                            Self::#v_ident(#(#names),*) => {
                                0 #(+ #additions)*
                            }
                        }
                    }
                    Fields::Unit => quote! {
                        Self::#v_ident => 0,
                    },
                }
            });
            quote! {
                match self {
                    #(#arms)*
                }
            }
        }
        Data::Union(_) => {
            return Err(Error::new_spanned(input, "unions are not supported"));
        }
    };

    Ok(quote! {
        #[automatically_derived]
        impl #impl_generics #wasmtime_crate::component::HostHeapUsage for #ident #ty_generics #where_clause {
            fn heap_size_of_children(&self) -> usize {
                #body
            }
        }
    })
}

pub fn expand_fixed_host_heap_usage(input: &DeriveInput) -> Result<TokenStream> {
    let wasmtime_crate = find_wasmtime_crate(&input.attrs)?;
    let ident = &input.ident;
    let (impl_generics, ty_generics, where_clause) = input.generics.split_for_impl();

    let mut where_clause = where_clause.cloned().unwrap_or_else(|| parse_quote!(where));
    let field_types: Vec<&syn::Type> = match &input.data {
        Data::Struct(data) => data.fields.iter().map(|f| &f.ty).collect(),
        Data::Enum(data) => data
            .variants
            .iter()
            .flat_map(|v| v.fields.iter().map(|f| &f.ty))
            .collect(),
        Data::Union(_) => return Err(Error::new_spanned(input, "unions are not supported")),
    };

    for ty in field_types {
        where_clause.predicates.push(parse_quote!(
            #ty: #wasmtime_crate::component::FixedHostHeapUsage
        ));
    }

    Ok(quote! {
        #[automatically_derived]
        impl #impl_generics #wasmtime_crate::component::FixedHostHeapUsage for #ident #ty_generics #where_clause {}
    })
}
