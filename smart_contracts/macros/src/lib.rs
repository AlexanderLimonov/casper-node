extern crate proc_macro;

use darling::{FromAttributes, FromDeriveInput, FromMeta};
use proc_macro::{Literal, TokenStream};
use proc_macro2::Span;
use quote::{format_ident, quote};
use syn::{
    parse_macro_input, token::Struct, Data, DeriveInput, Error, Fields, ItemEnum, ItemFn, ItemImpl,
    ItemStruct, ItemUnion, Meta, Path, Type,
};
use vm_common::flags::{self, EntryPointFlags};

#[derive(Debug, FromAttributes)]
#[darling(attributes(casper))]
struct MethodAttribute {
    #[darling(default)]
    constructor: bool,
    #[darling(default)]
    revert_on_error: bool,
}

#[proc_macro_derive(Contract)]
pub fn derive_casper_contract(input: TokenStream) -> TokenStream {
    // Parse the input tokens into a syntax tree
    // let input = parse_macro_input!(input as DeriveInput);
    let contract = parse_macro_input!(input as DeriveInput);
    // todo!("{contract:?}")
    let name = &contract.ident;
    let _vis = &contract.vis;

    let data_struct = match &contract.data {
        Data::Struct(s) => s,
        Data::Enum(_) => todo!("Enum"),
        Data::Union(_) => todo!("Union"),
    };

    let f = quote! {

        impl casper_sdk::Contract for #name {
            fn name() -> &'static str {
                stringify!(#name)
            }

            fn create(entry_point: Option<&str>, input_data: Option<&[u8]>) -> Result<casper_sdk::sys::CreateResult, casper_sdk::types::CallError> {
                Self::__casper_create(entry_point, input_data)
            }
        }
    };
    f.into()
}

#[proc_macro_attribute]
pub fn casper(attrs: TokenStream, item: TokenStream) -> TokenStream {
    let mut attrs_iter = attrs.into_iter().peekable();

    for attr in &mut attrs_iter {
        let item = item.clone();
        match attr {
            proc_macro::TokenTree::Ident(ident) if ident.to_string() == "entry_points" => {
                let mut populate_definitions = Vec::new();

                let mut entry_points = parse_macro_input!(item as ItemImpl);

                let struct_name = match entry_points.self_ty.as_ref() {
                    Type::Path(ref path) => &path.path,

                    other => todo!("{other:?}"),
                };
                // todo!("{:?}", &entry_points);
                // let name = entry_points

                // }

                let mut defs = Vec::new();

                let mut names = Vec::new();

                let mut manifest_entry_points_data = Vec::new();
                let mut manifest_entry_points = Vec::new();
                let mut manifest_entry_point_enum_variants = Vec::new();
                let mut manifest_entry_point_enum_match_name = Vec::new();
                let mut manifest_entry_point_input_data = Vec::new();

                for entry_point in &mut entry_points.items {
                    let method_attribute;
                    let mut flag_value = EntryPointFlags::empty();

                    let func = match entry_point {
                        syn::ImplItem::Const(_) => todo!(),
                        syn::ImplItem::Fn(ref mut func) => {
                            // TODO: Can we use darling to parse this in a sane way?
                            method_attribute =
                                MethodAttribute::from_attributes(&func.attrs).unwrap();
                            func.attrs.clear();

                            let name = func.sig.ident.clone();
                            names.push(name.clone());

                            let arg_names_and_types = func
                                .sig
                                .inputs
                                .iter()
                                .filter_map(|arg| match arg {
                                    syn::FnArg::Receiver(_) => None,
                                    syn::FnArg::Typed(typed) => match typed.pat.as_ref() {
                                        syn::Pat::Ident(ident) => Some((&ident.ident, &typed.ty)),
                                        _ => todo!(),
                                    },
                                })
                                .collect::<Vec<_>>();

                            let arg_names: Vec<_> =
                                arg_names_and_types.iter().map(|(name, _ty)| name).collect();
                            let arg_types: Vec<_> =
                                arg_names_and_types.iter().map(|(_name, ty)| ty).collect();

                            let arg_count = arg_names.len();

                            let preamble = if method_attribute.constructor {
                                let sig = &func.sig;
                                match func.sig.inputs.first() {
                                    Some(syn::FnArg::Receiver(receiver)) => {
                                        panic!("Constructor should not take a receiver")
                                    }
                                    _ => {}
                                }
                                match &func.sig.output {
                                    syn::ReturnType::Default => {
                                        panic!(
                                            "Constructor should return an instance of the struct"
                                        );
                                    }
                                    syn::ReturnType::Type(_, ty) => match ty.as_ref() {
                                        Type::Never(_) => {
                                            panic!("Constructors should return a have return value")
                                        }
                                        ty2 => {
                                            quote! {
                                                static_assertions::assert_type_eq_all(#struct_name, #ty2);
                                            }
                                        }
                                    },
                                }
                            } else {
                                quote! {}
                            };

                            assert_eq!(arg_names.len(), arg_types.len());

                            let mut entrypoint_params = Vec::new();

                            for (name, ty) in &arg_names_and_types {
                                entrypoint_params.push(quote! {
                                    {
                                        casper_sdk::sys::Param {
                                            name_ptr: stringify!(#name).as_ptr(),
                                            name_len: stringify!(#name).len(),
                                        }
                                    }
                                });
                            }

                            // let wrapper_function_name = format_ident!("")

                            if method_attribute.constructor {
                                manifest_entry_points_data.push(quote! {

                                    #[allow(non_upper_case_globals)]
                                    const #name: (&'static str, [casper_sdk::sys::Param; #arg_count], extern "C" fn() -> ()) = {
                                        extern "C" fn #name() {
                                            let _ret = casper_sdk::host::start(|(#(#arg_names,)*):(#(#arg_types,)*)| {
                                                <#struct_name>::#name(#(#arg_names,)*)
                                            });
                                        }
                                        (stringify!(#name), [#(#entrypoint_params,)*], #name)
                                    };

                                });
                            } else {
                                let handle_err = if method_attribute.revert_on_error {
                                    quote! {
                                        let _ret: &Result<_, _> = &_ret;
                                        if _ret.is_err() {
                                            flags |= vm_common::flags::ReturnFlags::REVERT;
                                        }
                                    }
                                } else {
                                    quote! {}
                                };

                                manifest_entry_points_data.push(quote! {

                                    #[allow(non_upper_case_globals)]
                                    const #name: (&'static str, [casper_sdk::sys::Param; #arg_count], extern "C" fn() -> ()) = {
                                        extern "C" fn #name() {
                                            let mut flags = vm_common::flags::ReturnFlags::empty();
                                            let mut instance: #struct_name = casper_sdk::host::read_state().unwrap();
                                            let _ret = casper_sdk::host::start_noret(|(#(#arg_names,)*):(#(#arg_types,)*)| {
                                                instance.#name(#(#arg_names,)*)
                                            });
                                            #handle_err;
                                            casper_sdk::host::write_state(&instance).unwrap();
                                            let ret_bytes = borsh::to_vec(&_ret).unwrap();
                                            casper_sdk::host::casper_return(flags, Some(&ret_bytes));
                                        }
                                        (stringify!(#name), [#(#entrypoint_params,)*], #name)
                                    };

                                });
                            }

                            if method_attribute.constructor {
                                flag_value |= EntryPointFlags::CONSTRUCTOR;
                            }

                            let bits = flag_value.bits();

                            manifest_entry_points.push(quote! {
                                {
                                    casper_sdk::sys::EntryPoint {
                                        name_ptr: #name.0.as_ptr(),
                                        name_len: #name.0.len(),
                                        params_ptr: #name.1.as_ptr(),
                                        params_size: #name.1.len(),
                                        fptr: #name.2,
                                        flags: #bits,
                                    }
                                }
                            });

                            manifest_entry_point_enum_variants.push(quote! {
                                #name {
                                    #(#arg_names: #arg_types,)*
                                }
                            });

                            manifest_entry_point_enum_match_name.push(quote! {
                                #name
                            });

                            manifest_entry_point_input_data.push(quote! {
                                Self::#name { #(#arg_names,)* } => {
                                    let into_tuple = (#(#arg_names,)*);
                                    into_tuple.serialize(writer)
                                }
                            });

                            func.clone()
                        }
                        syn::ImplItem::Type(_) => todo!(),
                        syn::ImplItem::Macro(_) => todo!(),
                        syn::ImplItem::Verbatim(_) => todo!(),
                        _ => todo!(),
                    };

                    let func_name = &func.sig.ident;

                    let result = match &func.sig.output {
                        syn::ReturnType::Default => {
                            populate_definitions.push(quote! {
                                definitions.populate_one::<()>();
                            });

                            quote! { <() as casper_sdk::abi::CasperABI>::declaration() }
                        }
                        syn::ReturnType::Type(_, ty) => match ty.as_ref() {
                            Type::Never(_) => {
                                populate_definitions.push(quote! {
                                    definitions.populate_one::<()>();
                                });

                                quote! { <() as casper_sdk::abi::CasperABI>::declaration() }
                            }
                            _ => {
                                populate_definitions.push(quote! {
                                    definitions.populate_one::<#ty>();
                                });

                                quote! { <#ty as casper_sdk::abi::CasperABI>::declaration() }
                            }
                        },
                    };

                    let mut args = Vec::new();

                    for input in &func.sig.inputs {
                        let typed = match input {
                            syn::FnArg::Receiver(_receiver) => continue,
                            syn::FnArg::Typed(typed) => typed,
                        };
                        // typed
                        let name = match &typed.pat.as_ref() {
                            syn::Pat::Const(_) => todo!("Const"),
                            syn::Pat::Ident(ident) => ident,
                            syn::Pat::Lit(_) => todo!("Lit"),
                            syn::Pat::Macro(_) => todo!("Macro"),
                            syn::Pat::Or(_) => todo!("Or"),
                            syn::Pat::Paren(_) => todo!("Paren"),
                            syn::Pat::Path(_) => todo!("Path"),
                            syn::Pat::Range(_) => todo!("Range"),
                            syn::Pat::Reference(_) => todo!("Reference"),
                            syn::Pat::Rest(_) => todo!("Rest"),
                            syn::Pat::Slice(_) => todo!("Slice"),
                            syn::Pat::Struct(_) => todo!("Struct"),
                            syn::Pat::Tuple(_) => todo!("Tuple"),
                            syn::Pat::TupleStruct(_) => todo!("TupleStruct"),
                            syn::Pat::Type(_) => todo!("Type"),
                            syn::Pat::Verbatim(_) => todo!("Verbatim"),
                            syn::Pat::Wild(_) => todo!("Wild"),
                            _ => todo!(),
                        };
                        let ty = &typed.ty;

                        populate_definitions.push(quote! {
                            definitions.populate_one::<#ty>();
                        });

                        args.push(quote! {
                            casper_sdk::schema::SchemaArgument {
                                name: stringify!(#name).into(),
                                decl: <#ty as casper_sdk::abi::CasperABI>::declaration(),
                            }
                        });
                    }

                    // let mut args = Vec::new();
                    // for arg in &entry_point
                    let bits = flag_value.bits();

                    defs.push(quote! {
                        casper_sdk::schema::SchemaEntryPoint {
                            name: stringify!(#func_name).into(),
                            arguments: vec![ #(#args,)* ],
                            result: #result,
                            flags: vm_common::flags::EntryPointFlags::from_bits(#bits).unwrap(),
                        }
                    });
                }

                // Create a expansion token from the length of `manifest_entry_points_data`
                let manifest_entry_points_data_len = manifest_entry_points_data.len();

                let st_name = struct_name.get_ident().unwrap();

                let res = quote! {
                    #entry_points

                    impl #struct_name {
                        #[doc(hidden)]
                        fn __casper_schema() -> casper_sdk::schema::Schema {
                            const VERSION: &str = env!("CARGO_PKG_VERSION");

                            let entry_points = vec![
                                #(#defs,)*
                            ];

                            let definitions = {
                                let mut definitions = casper_sdk::abi::Definitions::default();
                                <#struct_name as casper_sdk::abi::CasperABI>::populate_definitions(&mut definitions);
                                #(#populate_definitions)*;
                                definitions
                            };

                            let state = <#struct_name as casper_sdk::abi::CasperABI>::declaration();
                            casper_sdk::schema::Schema {
                                name: stringify!(#struct_name).into(),
                                version: Some(VERSION.into()),
                                definitions,
                                state,
                                entry_points,
                            }
                        }

                        #[inline(always)]
                        #[doc(hidden)]
                        fn __casper_create(entry_point: Option<&str>, input_data: Option<&[u8]>) -> Result<casper_sdk::sys::CreateResult, casper_sdk::types::CallError> {
                            #(#manifest_entry_points_data)*;
                            const ENTRY_POINTS: [casper_sdk::sys::EntryPoint; #manifest_entry_points_data_len] = [#(#manifest_entry_points,)*];

                            const MANIFEST: casper_sdk::sys::Manifest = casper_sdk::sys::Manifest {
                                entry_points: ENTRY_POINTS.as_ptr(),
                                entry_points_size: ENTRY_POINTS.len(),
                            };

                            casper_sdk::host::casper_create(None, &MANIFEST, entry_point, input_data)
                        }
                    }
                };

                return res.into();
            }
            proc_macro::TokenTree::Ident(ident) if ident.to_string() == "contract" => {
                todo!()
            }
            proc_macro::TokenTree::Ident(ident) if ident.to_string() == "export" => {
                let func = parse_macro_input!(item as ItemFn);
                let func_name = &func.sig.ident;

                // let mut arg_slices = Vec::new();
                // let mut arg_casts = Vec::new();
                let mut arg_names = Vec::new();
                let mut arg_types = Vec::new();
                // let mut tuple_args = Vec::new::new();

                for input in &func.sig.inputs {
                    let (name, ty) = match input {
                        syn::FnArg::Receiver(receiver) => {
                            todo!("{receiver:?}")
                        }
                        syn::FnArg::Typed(typed) => match typed.pat.as_ref() {
                            syn::Pat::Ident(ident) => (&ident.ident, &typed.ty),
                            _ => todo!(),
                        },
                    };
                    // let ty = &input.
                    arg_names.push(name.clone());
                    arg_types.push(ty.clone());

                    // let sig = &input.sig.inputs;
                    // // let name = match typed.pat.as_ref() {
                    // //     syn::Pat::Ident(ident) => &ident.ident,
                    // //     _ => todo!(),
                    // // };

                    // // let name = input.n
                    // let arg = quote! {
                    //     unsafe { core::ptr::NonNull::new_unchecked(#name).as_ref() }.as_slice()
                    // };

                    // arg_casts.push(arg);
                    // let arg_slice = quote! {
                    //     #name: *mut casper_sdk::host::Slice
                    // };
                    // arg_slices.push(arg_slice);

                    // arg_calls.push(quote! {
                    //     name
                    // })
                }

                // let arg_tokens =
                // let

                // let inputs = &func.sig.inputs;
                // Ident::
                let mod_name = format_ident!("__casper__export_{func_name}");
                let ctor_name = format_ident!("{func_name}_ctor");
                // let export_name = format_ident!("NAME_{func_name}");
                // let export_args = format_ident!("ARGS_{func_name}");

                let token = quote! {
                    pub(crate) mod #mod_name {
                        use super::*;
                        use borsh::BorshDeserialize;

                        // #[cfg(not(target_arch = "wasm32")]
                        // use ctor::ctor;

                        #func

                    }

                    #[cfg(target_arch = "wasm32")]
                    #[no_mangle]
                    pub extern "C" fn #func_name() {
                        // Set panic hook (assumes std is enabled etc.)
                        casper_sdk::set_panic_hook();

                        // TODO: If signature has no args we don't need to deserialize anything
                        use borsh::BorshDeserialize;

                        // ("foo", 1234) -> input

                        let input = casper_sdk::host::casper_copy_input();
                        // let args = #mod_name::Arguments::try_from_slice(&input).unwrap();
                        let ( #(#arg_names,)* ) = BorshDeserialize::try_from_slice(&input).unwrap();

                        #mod_name::#func_name(#(#arg_names,)*);
                    }

                    #[cfg(not(target_arch = "wasm32"))]
                    fn #func_name(input: &[u8]) {
                        use borsh::BorshDeserialize;
                        let ( #(#arg_names,)* ) = BorshDeserialize::try_from_slice(input).unwrap();
                        #mod_name::#func_name(#(#arg_names,)*);
                    }

                    #[cfg(not(target_arch = "wasm32"))]
                    #[ctor::ctor]
                    fn #ctor_name() {
                        let export = casper_sdk::schema::schema_helper::Export {
                            name: stringify!(#func_name),
                            fptr: #func_name,
                        };

                        casper_sdk::schema::schema_helper::register_export(export);
                    }
                    // #[cfg(not(target_arch = "wasm32"))]
                    // pub use #mod_name::{#func_name};

                    // #[cfg(not(target_arch = "wasm32"))]

                    // pub fn #func_name(input: Vec<u8>) {
                    //     use borsh::BorshDeserialize;
                    //     let ( #(#arg_names,)* ) = BorshDeserialize::try_from_slice(&input).unwrap();
                    //     #mod_name::#func_name(#(arg_names,)*);
                    // }
                };

                return token.into();
            }
            other => todo!("other attribute {other:?}"),
        }
    }
    todo!()
}

#[proc_macro_attribute]
pub fn entry_point(_attr: TokenStream, item: TokenStream) -> TokenStream {
    let func = parse_macro_input!(item as ItemFn);

    let vis = &func.vis;
    let _sig = &func.sig;
    let func_name = &func.sig.ident;

    let block = &func.block;

    let mut handle_args = Vec::new();
    let mut params = Vec::new();

    for arg in &func.sig.inputs {
        let typed = match arg {
            syn::FnArg::Receiver(_) => todo!(),
            syn::FnArg::Typed(typed) => typed,
        };

        let name = match typed.pat.as_ref() {
            syn::Pat::Ident(ident) => &ident.ident,
            _ => todo!(),
        };

        let ty = &typed.ty;

        let tok = quote! {
            let #typed = casper_sdk::get_named_arg(stringify!(#name)).expect("should get named arg");
        };
        handle_args.push(tok);

        let tok2 = quote! {
            (stringify!(#name), <#ty>::cl_type())
        };
        params.push(tok2);
    }

    // let len = params.len();

    let output = &func.sig.output;

    // let const_tok =

    let gen = quote! {
        // const paste!(#func_name, _ENTRY_POINT): &str = #func_name;

        #vis fn #func_name() {
            #(#handle_args)*;

            let closure = || #output {
                #block
            };

            let result = closure();

            // casper_sdk::EntryPoint {
            //     name: #func_name,
            //     params: &[
            //         #(#params,)*
            //     ],
            //     func: closure,
            // }

            result.expect("should work")
        }
    };

    println!("{}", gen);

    // quote!(fn foo() {})
    // item
    gen.into()
}

const PRIMITIVE_TYPES: &[&str] = &[
    "u8", "u16", "u32", "u64", "u128", "i8", "i16", "i32", "i64", "i128", "bool", "String", "f32",
    "f64",
];

#[proc_macro_derive(CasperSchema, attributes(casper))]
pub fn derive_casper_schema(input: TokenStream) -> TokenStream {
    let contract = parse_macro_input!(input as DeriveInput);
    let data_struct = match &contract.data {
        Data::Struct(s) => s,
        Data::Enum(_) => todo!("Enum"),
        Data::Union(_) => todo!("Union"),
    };

    let name = &contract.ident;

    quote! {
        impl casper_sdk::schema::CasperSchema for #name {
            fn schema() -> casper_sdk::schema::Schema {
                Self::__casper_schema()
            }
        }
    }
    .into()
}

#[proc_macro_derive(CasperABI, attributes(casper))]
pub fn derive_casper_abi(input: TokenStream) -> TokenStream {
    let res = if let Ok(input) = syn::parse::<ItemStruct>(input.clone()) {
        let mut populate_definitions = Vec::new();
        let name = input.ident.clone();
        let mut items = Vec::new();
        for field in input.fields.iter() {
            match &field.ty {
                Type::Path(path) => {
                    for segment in &path.path.segments {
                        let field_name = &field.ident;

                        populate_definitions.push(quote! {
                            definitions.populate_one::<#segment>();
                        });

                        items.push(quote! {
                            casper_sdk::abi::StructField {
                                name: stringify!(#field_name).into(),
                                decl: <#segment>::declaration(),
                            }
                        })
                    }
                }
                other_ty => todo!("Unsupported type {other_ty:?}"),
            }
        }

        Ok(quote! {
            impl casper_sdk::abi::CasperABI for #name {
                fn populate_definitions(definitions: &mut casper_sdk::abi::Definitions) {
                    #(#populate_definitions)*;
                }

                fn declaration() -> casper_sdk::abi::Declaration {
                    format!("{}::{}", module_path!(), stringify!(#name))
                }

                fn definition() -> casper_sdk::abi::Definition {
                    casper_sdk::abi::Definition::Struct {
                        items: vec![
                            #(#items,)*
                        ]
                    }
                }
            }
        })
    } else if let Ok(input) = syn::parse::<ItemEnum>(input.clone()) {
        // TODO: Check visibility
        let name = input.ident.clone();

        let mut all_definitions = Vec::new();
        let mut all_variants = Vec::new();
        let mut populate_definitions = Vec::new();
        let mut has_unit_definition = false;

        // populate_definitions.push(quote! {
        //     definitions.populate_one::<#name>();
        // });

        all_definitions.push(quote! {
            casper_sdk::abi::Definition::Enum {
                name: stringify!(#name).into(),
            }
        });

        let mut current_discriminant = 0;

        for variant in input.variants.iter() {
            if let Some(discriminant) = &variant.discriminant {
                match &discriminant.1 {
                    syn::Expr::Lit(lit) => match &lit.lit {
                        syn::Lit::Int(int) => {
                            current_discriminant = int.base10_parse::<u64>().unwrap();
                        }
                        _ => todo!(),
                    },
                    _ => todo!(),
                }
            }

            let variant_name = &variant.ident;

            let variant_decl = match &variant.fields {
                Fields::Unit => {
                    // NOTE: Generate an empty struct here for a definition.
                    if !has_unit_definition {
                        populate_definitions.push(quote! {
                            definitions.populate_one::<()>();
                        });
                        has_unit_definition = true;
                    }

                    quote! {
                        <()>::declaration()
                    }
                }
                Fields::Named(named) => {
                    let mut fields = Vec::new();

                    let variant_name = format_ident!("{name}_{variant_name}");

                    for field in &named.named {
                        let field_name = &field.ident;
                        match &field.ty {
                            Type::Path(path) => {
                                populate_definitions.push(quote! {
                                    definitions.populate_one::<#path>();
                                });

                                fields.push(quote! {
                                    casper_sdk::abi::StructField {
                                        name: stringify!(#field_name).into(),
                                        decl: <#path as casper_sdk::abi::CasperABI>::declaration()
                                    }
                                });
                            }
                            other_ty => todo!("Unsupported type {other_ty:?}"),
                        }
                    }

                    populate_definitions.push(quote! {
                        definitions.populate_custom(
                            stringify!(#variant_name).into(),
                            casper_sdk::abi::Definition::Struct {
                                items: vec![
                                    #(#fields,)*
                                ],
                            });
                    });

                    quote! {
                        stringify!(#variant_name).into()
                    }
                }
                Fields::Unnamed(unnamed_fields) => {
                    let mut fields = Vec::new();

                    let variant_name = format_ident!("{name}_{variant_name}");

                    for field in &unnamed_fields.unnamed {
                        match &field.ty {
                            Type::Path(path) => {
                                for segment in &path.path.segments {
                                    let type_name = &segment.ident;
                                    populate_definitions.push(quote! {
                                        definitions.populate_one::<#type_name>();
                                    });

                                    fields.push(quote! {
                                        <#type_name as casper_sdk::abi::CasperABI>::declaration()
                                    });
                                }
                            }
                            other_ty => todo!("Unsupported type {other_ty:?}"),
                        }
                    }

                    populate_definitions.push(quote! {
                        definitions.populate_custom(
                            stringify!(#variant_name).into(),
                            casper_sdk::abi::Definition::Tuple {
                                items: vec![
                                    #(#fields,)*
                                ],
                            });
                    });

                    quote! {
                        stringify!(#variant_name).into()
                    }
                }
            };

            all_variants.push(quote! {
                casper_sdk::abi::EnumVariant {
                    name: stringify!(#variant_name).into(),
                    discriminant: #current_discriminant,
                    decl: #variant_decl,
                }
            });

            current_discriminant += 1;
        }

        Ok(quote! {
            impl casper_sdk::abi::CasperABI for #name {
                fn populate_definitions(definitions: &mut casper_sdk::abi::Definitions) {
                    #(#populate_definitions)*;
                }

                fn declaration() -> casper_sdk::abi::Declaration {
                    format!("{}::{}", module_path!(), stringify!(#name))
                }

                fn definition() -> casper_sdk::abi::Definition {
                    casper_sdk::abi::Definition::Enum {
                        items: vec![
                            #(#all_variants,)*
                        ],
                    }
                }
            }
        })
    } else if syn::parse::<ItemUnion>(input).is_ok() {
        Err(syn::Error::new(
            Span::call_site(),
            "Borsh schema does not support unions yet.",
        ))
    } else {
        // Derive macros can only be defined on structs, enums, and unions.
        unreachable!()
    };
    TokenStream::from(match res {
        Ok(res) => res,
        Err(err) => err.to_compile_error(),
    })
}
