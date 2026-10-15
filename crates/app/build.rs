//! The speech libraries (sherpa-onnx, onnxruntime) are shared libraries that sit next to
//! the executable. Windows finds them there on its own; Linux needs to be told.

fn main() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("linux") {
        println!("cargo:rustc-link-arg-bins=-Wl,-rpath,$ORIGIN");
    }
}
