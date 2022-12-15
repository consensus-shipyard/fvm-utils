pub use fil_actors_runtime as runtime;
pub use primitives;
pub use interface_trait::StructSignature;


#[test]
fn demo_struct_signature_derive() {
    #[derive(StructSignature)]
    #[allow(dead_code)]
    struct Foo{
        pub test: String,
        pub test2: String,
        pub test3: Option<String>
    }

    println!("{:?}", Foo::signature());
}