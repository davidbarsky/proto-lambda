extern crate proc_macro;

use proc_macro::TokenStream;
use quote::{quote, quote_spanned};
use syn::{spanned::Spanned, Expr, FnArg, ItemFn};

#[cfg(not(test))]
#[proc_macro_attribute]
pub fn main(_attr: TokenStream, item: TokenStream) -> TokenStream {
    let rt: Expr = syn::parse_str("runtime::native::Native").unwrap();
    let input = syn::parse_macro_input!(item as ItemFn);

    let ret = &input.decl.output;
    let name = &input.ident;
    let body = &input.block;
    let attrs = &input.attrs;
    let inputs = &input.decl.inputs;

    if name != "main" {
        let tokens = quote_spanned! { name.span() =>
            compile_error!("only the main function can be tagged with #[lambda::main]");
        };
        return TokenStream::from(tokens);
    }

    if input.asyncness.is_none() {
        let tokens = quote_spanned! { input.span() =>
          compile_error!("the async keyword is missing from the function declaration");
        };
        return TokenStream::from(tokens);
    }

    let result = match inputs.len() {
        0 => {
            quote_spanned! { input.span() =>
                #(#attrs)*
                fn main() #ret {
                    async fn main(#(#inputs),*) #ret {
                        #body
                    }

                    runtime::raw::enter(#rt, async {
                        main().await
                    })
                }
            }
        }
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
                fn main() #ret {
                    async fn main(#arg_name: Option<#arg_type>) #ret {
                        #body
                    }

                    runtime::raw::enter(#rt, async move {
                        main(None).await
                    })
                }
            }
        }
        _ => {
            let tokens = quote_spanned! { inputs.span() =>
                compile_error!("fn main can take 0 or 1 arguments");
            };
            return TokenStream::from(tokens);
        }
    };

    result.into()
}
