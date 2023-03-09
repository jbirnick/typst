use super::*;

/// Expand the `#[node]` macro.
pub fn node(stream: TokenStream, body: syn::ItemStruct) -> Result<TokenStream> {
    let node = prepare(stream, &body)?;
    Ok(create(&node))
}

struct Node {
    attrs: Vec<syn::Attribute>,
    vis: syn::Visibility,
    ident: Ident,
    name: String,
    capable: Vec<Ident>,
    fields: Vec<Field>,
}

impl Node {
    fn inherent(&self) -> impl Iterator<Item = &Field> {
        self.fields.iter().filter(|field| field.inherent())
    }

    fn settable(&self) -> impl Iterator<Item = &Field> {
        self.fields.iter().filter(|field| field.settable())
    }
}

struct Field {
    attrs: Vec<syn::Attribute>,
    vis: syn::Visibility,

    name: String,
    ident: Ident,
    ident_in: Ident,
    with_ident: Ident,
    set_ident: Ident,

    internal: bool,
    positional: bool,
    required: bool,
    variadic: bool,
    fold: bool,
    resolve: bool,
    parse: Option<FieldParser>,

    ty: syn::Type,
    output: syn::Type,
    default: syn::Expr,
}

impl Field {
    fn inherent(&self) -> bool {
        self.required || self.variadic
    }

    fn settable(&self) -> bool {
        !self.inherent()
    }
}

struct FieldParser {
    prefix: Vec<syn::Stmt>,
    expr: syn::Stmt,
}

impl Parse for FieldParser {
    fn parse(input: ParseStream) -> Result<Self> {
        let mut stmts = syn::Block::parse_within(input)?;
        let Some(expr) = stmts.pop() else {
            return Err(input.error("expected at least on expression"));
        };
        Ok(Self { prefix: stmts, expr })
    }
}

/// Preprocess the node's definition.
fn prepare(stream: TokenStream, body: &syn::ItemStruct) -> Result<Node> {
    let syn::Fields::Named(named) = &body.fields else {
        bail!(body, "expected named fields");
    };

    let mut fields = vec![];
    for field in &named.named {
        let Some(ident) = field.ident.clone() else {
            bail!(field, "expected named field");
        };

        let mut attrs = field.attrs.clone();
        let variadic = has_attr(&mut attrs, "variadic");

        let mut field = Field {
            vis: field.vis.clone(),

            name: kebab_case(&ident),
            ident: ident.clone(),
            ident_in: Ident::new(&format!("{}_in", ident), ident.span()),
            with_ident: Ident::new(&format!("with_{}", ident), ident.span()),
            set_ident: Ident::new(&format!("set_{}", ident), ident.span()),

            internal: has_attr(&mut attrs, "internal"),
            positional: has_attr(&mut attrs, "positional") || variadic,
            required: has_attr(&mut attrs, "required") || variadic,
            variadic,
            fold: has_attr(&mut attrs, "fold"),
            resolve: has_attr(&mut attrs, "resolve"),
            parse: parse_attr(&mut attrs, "parse")?.flatten(),

            ty: field.ty.clone(),
            output: field.ty.clone(),
            default: parse_attr(&mut attrs, "default")?
                .flatten()
                .unwrap_or_else(|| parse_quote! { ::std::default::Default::default() }),

            attrs: {
                validate_attrs(&attrs)?;
                attrs
            },
        };

        if field.required && (field.fold || field.resolve) {
            bail!(ident, "required fields cannot be folded or resolved");
        }

        if field.required && !field.positional {
            bail!(ident, "only positional fields can be required");
        }

        if field.resolve {
            let output = &field.output;
            field.output = parse_quote! { <#output as ::typst::model::Resolve>::Output };
        }
        if field.fold {
            let output = &field.output;
            field.output = parse_quote! { <#output as ::typst::model::Fold>::Output };
        }

        fields.push(field);
    }

    let capable = Punctuated::<Ident, Token![,]>::parse_terminated
        .parse2(stream)?
        .into_iter()
        .collect();

    let attrs = body.attrs.clone();
    Ok(Node {
        vis: body.vis.clone(),
        ident: body.ident.clone(),
        name: body.ident.to_string().trim_end_matches("Node").to_lowercase(),
        capable,
        fields,
        attrs: {
            validate_attrs(&attrs)?;
            attrs
        },
    })
}

/// Produce the node's definition.
fn create(node: &Node) -> TokenStream {
    let attrs = &node.attrs;
    let vis = &node.vis;
    let ident = &node.ident;

    // Inherent methods and functions.
    let new = create_new_func(node);
    let field_methods = node.fields.iter().map(create_field_method);
    let field_in_methods = node.settable().map(create_field_in_method);
    let with_fields_methods = node.fields.iter().map(create_with_field_method);
    let field_style_methods = node.settable().map(create_set_field_method);

    // Trait implementations.
    let construct = node
        .capable
        .iter()
        .all(|capability| capability != "Construct")
        .then(|| create_construct_impl(node));
    let set = create_set_impl(node);
    let node = create_node_impl(node);

    quote! {
        #(#attrs)*
        #[::typst::eval::func]
        #[derive(Debug, Clone, Hash)]
        #[repr(transparent)]
        #vis struct #ident(::typst::model::Content);

        impl #ident {
            #new
            #(#field_methods)*
            #(#field_in_methods)*
            #(#with_fields_methods)*
            #(#field_style_methods)*

            /// The node's span.
            pub fn span(&self) -> Option<::typst::syntax::Span> {
                self.0.span()
            }
        }

        #node
        #construct
        #set

        impl From<#ident> for ::typst::eval::Value {
            fn from(value: #ident) -> Self {
                value.0.into()
            }
        }
    }
}

/// Create the `new` function for the node.
fn create_new_func(node: &Node) -> TokenStream {
    let params = node.inherent().map(|Field { ident, ty, .. }| {
        quote! { #ident: #ty }
    });
    let builder_calls = node.inherent().map(|Field { ident, with_ident, .. }| {
        quote! { .#with_ident(#ident) }
    });
    quote! {
        /// Create a new node.
        pub fn new(#(#params),*) -> Self {
            Self(::typst::model::Content::new::<Self>())
            #(#builder_calls)*
        }
    }
}

/// Create an accessor methods for a field.
fn create_field_method(field: &Field) -> TokenStream {
    let Field { attrs, vis, ident, name, output, .. } = field;
    if field.inherent() {
        quote! {
            #(#attrs)*
            #vis fn #ident(&self) -> #output {
                self.0.cast_required_field(#name)
            }
        }
    } else {
        let access =
            create_style_chain_access(field, quote! { self.0.field(#name).cloned() });
        quote! {
            #(#attrs)*
            #vis fn #ident(&self, styles: ::typst::model::StyleChain) -> #output {
                #access
            }
        }
    }
}

/// Create a style chain access method for a field.
fn create_field_in_method(field: &Field) -> TokenStream {
    let Field { vis, ident_in, name, output, .. } = field;
    let doc = format!("Access the `{}` field in the given style chain.", name);
    let access = create_style_chain_access(field, quote! { None });
    quote! {
        #[doc = #doc]
        #vis fn #ident_in(styles: ::typst::model::StyleChain) -> #output {
            #access
        }
    }
}

/// Create a style chain access method for a field.
fn create_style_chain_access(field: &Field, inherent: TokenStream) -> TokenStream {
    let Field { name, ty, default, .. } = field;
    let getter = match (field.fold, field.resolve) {
        (false, false) => quote! { get },
        (false, true) => quote! { get_resolve },
        (true, false) => quote! { get_fold },
        (true, true) => quote! { get_resolve_fold },
    };

    quote! {
        styles.#getter::<#ty>(
            ::typst::model::NodeId::of::<Self>(),
            #name,
            #inherent,
            || #default,
        )
    }
}

/// Create a builder pattern method for a field.
fn create_with_field_method(field: &Field) -> TokenStream {
    let Field { vis, ident, with_ident, name, ty, .. } = field;
    let doc = format!("Set the [`{}`](Self::{}) field.", name, ident);
    quote! {
        #[doc = #doc]
        #vis fn #with_ident(mut self, #ident: #ty) -> Self {
            Self(self.0.with_field(#name, #ident))
        }
    }
}

/// Create a setter method for a field.
fn create_set_field_method(field: &Field) -> TokenStream {
    let Field { vis, ident, set_ident, name, ty, .. } = field;
    let doc = format!("Create a style property for the `{}` field.", name);
    quote! {
        #[doc = #doc]
        #vis fn #set_ident(#ident: #ty) -> ::typst::model::Property {
            ::typst::model::Property::new(
                ::typst::model::NodeId::of::<Self>(),
                #name.into(),
                #ident.into()
            )
        }
    }
}

/// Create the node's `Node` implementation.
fn create_node_impl(node: &Node) -> TokenStream {
    let ident = &node.ident;
    let name = &node.name;
    let vtable_func = create_vtable_func(node);
    let infos = node
        .fields
        .iter()
        .filter(|field| !field.internal)
        .map(create_param_info);
    quote! {
        impl ::typst::model::Node for #ident {
            fn pack(self) -> ::typst::model::Content {
                self.0
            }

            fn id() -> ::typst::model::NodeId {
                static META: ::typst::model::NodeMeta = ::typst::model::NodeMeta {
                    name: #name,
                    vtable: #vtable_func,
                };
                ::typst::model::NodeId::from_meta(&META)
            }

            fn params() -> ::std::vec::Vec<::typst::eval::ParamInfo> {
                ::std::vec![#(#infos),*]
            }
        }
    }
}

/// Create the node's casting vtable.
fn create_vtable_func(node: &Node) -> TokenStream {
    let ident = &node.ident;
    let relevant = node.capable.iter().filter(|&ident| ident != "Construct");
    let checks = relevant.map(|capability| {
        quote! {
            if id == ::std::any::TypeId::of::<dyn #capability>() {
                return Some(unsafe {
                    ::typst::util::fat::vtable(&null as &dyn #capability)
                });
            }
        }
    });

    quote! {
        |id| {
            let null = Self(::typst::model::Content::new::<#ident>());
            #(#checks)*
            None
        }
    }
}

/// Create a parameter info for a field.
fn create_param_info(field: &Field) -> TokenStream {
    let Field { name, positional, variadic, required, ty, .. } = field;
    let named = !positional;
    let settable = field.settable();
    let docs = documentation(&field.attrs);
    let docs = docs.trim();
    quote! {
        ::typst::eval::ParamInfo {
            name: #name,
            docs: #docs,
            cast: <#ty as ::typst::eval::Cast<
                ::typst::syntax::Spanned<::typst::eval::Value>
            >>::describe(),
            positional: #positional,
            named: #named,
            variadic: #variadic,
            required: #required,
            settable: #settable,
        }
    }
}

/// Create the node's `Construct` implementation.
fn create_construct_impl(node: &Node) -> TokenStream {
    let ident = &node.ident;
    let handlers = node
        .fields
        .iter()
        .filter(|field| !field.internal || field.parse.is_some())
        .map(|field| {
            let with_ident = &field.with_ident;
            let (prefix, value) = create_field_parser(field);
            if field.settable() {
                quote! {
                    #prefix
                    if let Some(value) = #value {
                        node = node.#with_ident(value);
                    }
                }
            } else {
                quote! {
                    #prefix
                    node = node.#with_ident(#value);
                }
            }
        });

    quote! {
        impl ::typst::model::Construct for #ident {
            fn construct(
                vm: &::typst::eval::Vm,
                args: &mut ::typst::eval::Args,
            ) -> ::typst::diag::SourceResult<::typst::model::Content> {
                let mut node = Self(::typst::model::Content::new::<Self>());
                #(#handlers)*
                Ok(node.0)
            }
        }
    }
}

/// Create the node's `Set` implementation.
fn create_set_impl(node: &Node) -> TokenStream {
    let ident = &node.ident;
    let handlers = node
        .fields
        .iter()
        .filter(|field| field.settable() && (!field.internal || field.parse.is_some()))
        .map(|field| {
            let set_ident = &field.set_ident;
            let (prefix, value) = create_field_parser(field);
            quote! {
                #prefix
                if let Some(value) = #value {
                    styles.set(Self::#set_ident(value));
                }
            }
        });

    quote! {
        impl ::typst::model::Set for #ident {
            fn set(
                args: &mut ::typst::eval::Args,
            ) -> ::typst::diag::SourceResult<::typst::model::StyleMap> {
                let mut styles = ::typst::model::StyleMap::new();
                #(#handlers)*
                Ok(styles)
            }
        }
    }
}

/// Create argument parsing code for a field.
fn create_field_parser(field: &Field) -> (TokenStream, TokenStream) {
    let name = &field.name;
    if let Some(FieldParser { prefix, expr }) = &field.parse {
        return (quote! { #(#prefix);* }, quote! { #expr });
    }

    let value = if field.variadic {
        quote! { args.all()? }
    } else if field.required {
        quote! { args.expect(#name)? }
    } else if field.positional {
        quote! { args.find()? }
    } else {
        quote! { args.named(#name)? }
    };

    (quote! {}, value)
}
