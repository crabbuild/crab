#[path = "../build/service_metadata.rs"]
mod service_metadata;

fn main() -> Result<(), std::env::VarError> {
    service_metadata::emit("../build/service_metadata.rs")
}
