//! `scope::require!` checks a scope written as a template, and
//! `#[scope::utoipa]` writes a handler's `require!` templates into its
//! OpenAPI `security`. Use both through the `scope` crate; see its README.

use proc_macro::TokenStream;
use proc_macro2::{TokenStream as Tokens, TokenTree};
use quote::{ToTokens, quote};
use syn::parse::{Parse, ParseStream};
use syn::{Error, Expr, Ident, ItemFn, LitStr, Token, parse_macro_input, parse_quote};

/// `require!(granted, "template", name = expr, …)`: returns
/// `Err(Denied.into())` unless `granted` covers the template's scope.
/// `{name}` takes a variable or named argument (`Display`), escaped as an
/// id; unparsable text is denied.
#[proc_macro]
pub fn require(input: TokenStream) -> TokenStream {
    let call = parse_macro_input!(input as Require);
    call.expand()
        .unwrap_or_else(Error::into_compile_error)
        .into()
}

/// `#[utoipa::path]` whose `security` is every `require!` template in the
/// body, in order, deduplicated, as written. Fails to compile on
/// `security(…)`, no `require!`, or a template not a string literal.
#[proc_macro_attribute]
pub fn utoipa(args: TokenStream, item: TokenStream) -> TokenStream {
    let function = parse_macro_input!(item as ItemFn);
    path(args.into(), &function)
        .map(|path| quote!(#path #function))
        .unwrap_or_else(Error::into_compile_error)
        .into()
}

/// A `require!` call's arguments.
struct Require {
    /// What the caller was granted.
    granted: Expr,
    /// The scope with `{name}` placeholders.
    template: LitStr,
    /// The `name = expr` arguments.
    named: Vec<(Ident, Expr)>,
}

impl Parse for Require {
    fn parse(input: ParseStream) -> syn::Result<Self> {
        let granted = input.parse()?;
        input.parse::<Token![,]>()?;
        let template = input.parse()?;
        let mut named = Vec::new();
        while input.parse::<Option<Token![,]>>()?.is_some() && !input.is_empty() {
            let name = input.parse()?;
            input.parse::<Token![=]>()?;
            named.push((name, input.parse()?));
        }
        Ok(Self {
            granted,
            template,
            named,
        })
    }
}

impl Require {
    /// The check: the filled text through `::scope::__require`.
    fn expand(self) -> Result<Tokens, Error> {
        let span = self.template.span();
        let text = self.template.value();
        let mut values = self.named;
        for name in placeholders(&text) {
            let mut ident: Ident = syn::parse_str(name)
                .map_err(|_| Error::new(span, format!("`{{{name}}}`: write `{{name}}`")))?;
            ident.set_span(span);
            if !values.iter().any(|(named, _)| *named == ident) {
                values.push((ident.clone(), parse_quote!(#ident)));
            }
        }
        let args = values.iter().map(|(name, value)| {
            quote!(#name = ::scope::__escape_id(&::std::string::ToString::to_string(&(#value))))
        });
        let (granted, template) = (self.granted, self.template);
        Ok(quote! {
            if let ::core::result::Result::Err(denied) = ::scope::__require(
                ::scope::Granted::scopes(&#granted),
                ::std::format!(#template, #(#args),*),
            ) {
                return ::core::result::Result::Err(::core::convert::From::from(denied));
            }
        })
    }
}

/// The names between `{` and `}` in `text`, `{{` skipped; `format!`
/// reports what else is wrong.
fn placeholders(text: &str) -> Vec<&str> {
    let (mut names, mut rest) = (Vec::new(), text);
    while let Some((_, after)) = rest.split_once('{') {
        rest = if let Some(escaped) = after.strip_prefix('{') {
            escaped
        } else {
            let Some((name, after)) = after.split_once('}') else {
                break;
            };
            names.push(name);
            after
        };
    }
    names
}

/// The `#[::utoipa::path]` attribute for `function`.
fn path(args: Tokens, function: &ItemFn) -> Result<Tokens, Error> {
    let is_security =
        |token: &TokenTree| matches!(token, TokenTree::Ident(name) if name == "security");
    if let Some(security) = args.clone().into_iter().find(is_security) {
        let why = "`security` comes from the body's `scope::require!` calls";
        return Err(Error::new_spanned(security, why));
    }
    let mut templates = Vec::new();
    templates_in(function.block.to_token_stream(), &mut templates)?;
    if templates.is_empty() {
        let why = "the body never calls `scope::require!`: an endpoint must check its scope";
        return Err(Error::new_spanned(&function.sig.ident, why));
    }
    let comma = (!args.is_empty()).then(|| quote!(,));
    Ok(quote!(#[::utoipa::path(#args #comma security(("bearer" = [#(#templates),*])))]))
}

/// Pushes the template of each `require!(…)` in `tokens` to `templates`,
/// unless already there.
fn templates_in(tokens: Tokens, templates: &mut Vec<String>) -> Result<(), Error> {
    let tokens: Vec<TokenTree> = tokens.into_iter().collect();
    for (at, token) in tokens.iter().enumerate() {
        match (token, tokens.get(at + 1), tokens.get(at + 2)) {
            (
                TokenTree::Ident(name),
                Some(TokenTree::Punct(bang)),
                Some(TokenTree::Group(call)),
            ) if name == "require" && bang.as_char() == '!' => {
                let template = syn::parse2::<Require>(call.stream())?.template.value();
                if !templates.contains(&template) {
                    templates.push(template);
                }
            }
            (TokenTree::Group(group), ..) => templates_in(group.stream(), templates)?,
            _ => {}
        }
    }
    Ok(())
}
