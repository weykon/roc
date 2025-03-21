//! Macros for use in `test_mono`.
extern crate proc_macro;

use proc_macro::TokenStream;
use quote::quote;

#[proc_macro_attribute]
pub fn mono_test(args: TokenStream, item: TokenStream) -> TokenStream {
    let mut no_check = false;
    let mut mode = "exec".to_owned();
    for arg in syn::parse_macro_input!(args as syn::AttributeArgs) {
        use syn::{Lit, Meta, MetaNameValue, NestedMeta};
        if let NestedMeta::Meta(Meta::NameValue(MetaNameValue {
            path,
            eq_token: _,
            lit: Lit::Str(s),
        })) = arg
        {
            if path.is_ident("mode") {
                mode = s.value();
            }
            if path.is_ident("no_check") {
                no_check = true;
            }
        }
    }

    let task_fn = syn::parse_macro_input!(item as syn::ItemFn);

    let args = task_fn.sig.inputs.clone();

    let name = task_fn.sig.ident.clone();
    let name_str = name.to_string();
    let body = task_fn.block.clone();

    let visibility = &task_fn.vis;
    let attributes = task_fn.attrs;

    let result = quote! {
        #[test]
        #(#attributes)*
        #visibility fn #name(#args) {
            compiles_to_ir(#name_str, #body, &#mode, #no_check);

        }
    };
    result.into()
}
