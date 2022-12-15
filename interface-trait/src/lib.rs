pub use interface_derive::StructSignature;
pub use blake2;
pub use hex;

pub trait StructSignature {
    const SIGNATURE_STR: &'static str;

    fn signature() -> String;
}

