fn main() {
    // Check if protoc is available
    let protoc_available = std::process::Command::new("protoc")
        .arg("--version")
        .output()
        .is_ok();

    if protoc_available {
        // Compile the protobuf file for gRPC tests
        match tonic_build::compile_protos("proto/echo.proto") {
            Ok(_) => {
                println!("cargo:warning=Successfully compiled protobuf definitions for gRPC tests");
            }
            Err(e) => {
                println!("cargo:warning=Failed to compile protobuf: {}", e);
                println!("cargo:warning=gRPC integration tests will not be available");
            }
        }
    } else {
        println!("cargo:warning=protoc not found - skipping gRPC test compilation");
        println!("cargo:warning=The main bridge will still build and work correctly");
        println!("cargo:warning=To enable gRPC integration tests, install protoc:");
        println!("cargo:warning=  - Debian/Ubuntu: apt-get install protobuf-compiler");
        println!("cargo:warning=  - Fedora: dnf install protobuf-compiler");
        println!("cargo:warning=  - macOS: brew install protobuf");
        println!("cargo:warning=  - Or download from: https://github.com/protocolbuffers/protobuf/releases");
    }

    // Tell Cargo to rerun this script if the proto file changes
    println!("cargo:rerun-if-changed=proto/echo.proto");
}
