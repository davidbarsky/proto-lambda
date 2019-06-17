extern crate proc_macro;

use proc_macro::TokenStream;
use quote::quote_spanned;
use syn::{spanned::Spanned, FnArg, ItemFn};

#[cfg(not(test))]
#[proc_macro_attribute]
pub fn lambda(_attr: TokenStream, item: TokenStream) -> TokenStream {
    let input = syn::parse_macro_input!(item as ItemFn);

    let ret = &input.decl.output;
    let name = &input.ident;
    let body = &input.block;
    let attrs = &input.attrs;
    let asyncness = &input.asyncness;
    let inputs = &input.decl.inputs;

    if name != "main" {
        let tokens = quote_spanned! { name.span() =>
            compile_error!("only the main function can be tagged with #[lambda::main]");
        };
        return TokenStream::from(tokens);
    }

    if asyncness.is_none() {
        let tokens = quote_spanned! { input.span() =>
          compile_error!("the async keyword is missing from the function declaration");
        };
        return TokenStream::from(tokens);
    }

    let result = match inputs.len() {
        1 => {
            let arg = match inputs.first().unwrap().into_value() {
                FnArg::Captured(arg) => arg,
                _ => {
                    let tokens = quote_spanned! { inputs.span() =>
                        compile_error!("fn main should take a fully formed argument");
                    };
                    return TokenStream::from(tokens);
                }
            };
            let arg_name = &arg.pat;
            let arg_type = &arg.ty;
            quote_spanned! { input.span() =>
                #(#attrs)*
                #asyncness fn main() {
                    async fn actual(#arg_name: #arg_type) #ret {
                        #body
                    }
                    let f = lambda::handler_fn(actual);

                    lambda::run(f).await.unwrap();
                }
            }
        }
        _ => {
            let tokens = quote_spanned! { inputs.span() =>
                compile_error!("fn main can only take 1 argument");
            };
            return TokenStream::from(tokens);
        }
    };

    result.into()
}
