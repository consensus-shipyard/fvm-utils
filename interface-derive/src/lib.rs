use proc_macro::{self, TokenStream};
use quote::quote;
use syn::{parse_macro_input, DeriveInput, Fields};

#[proc_macro_derive(StructSignature)]
pub fn derive(input: TokenStream) -> TokenStream {
    let i: DeriveInput = parse_macro_input!(input);
    impl_method_signature(&i)
}

fn impl_method_signature(ast: &syn::DeriveInput) -> TokenStream {
    let name = &ast.ident;

    // A failed experiment to access to the fields of MyStruct
    // The next line doesn't compiles
    let field_types = if let syn::Data::Struct(data) = &ast.data {
        let fields = &data.fields;

        // TODO: ideally we should be able to generate the hash in this function,
        // TODO: but somehow I just cannot get the `field.ty` to String or bytes
        // TODO: we should explore converting `field.ty` to bytes then we can generate
        // TODO: the hash offline. Anyone has any tips?
        match fields {
            Fields::Named(ref fields) => {
                fields.named.iter().map(|field| field.ty.clone()).collect::<Vec<_>>()
            }
            _ => {
                panic!("The StructSignature derive macro can only be applied to named fields.");
            }
        }
    }
    else {
        panic!("The StructSignature derive macro can only be applied to structs.");
    };

    let tokens = quote! {
        impl StructSignature for #name {
            const SIGNATURE_STR: &'static str = stringify!(#(#field_types),*);

            fn signature() -> String {
                use interface_trait::blake2::{Blake2b512, Digest};
                let mut hasher = Blake2b512::new();
                hasher.update(Self::SIGNATURE_STR);
                interface_trait::hex::encode(hasher.finalize().to_vec())
            }
        }
    };

    tokens.into()
}