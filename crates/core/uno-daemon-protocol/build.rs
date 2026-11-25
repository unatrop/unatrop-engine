fn main() {
    let protos = [
        "proto/common.proto",
        "proto/messages.proto",
        "proto/session.proto",
        "proto/ws.proto",
        "proto/call.proto",
    ];
    for file in &protos {
        println!("cargo:rerun-if-changed={file}");
    }

    let file_descriptors = protox::compile(protos, ["proto"])
        .expect("failed to compile uno-daemon proto files with protox");

    prost_build::Config::new()
        .compile_fds(file_descriptors)
        .expect("failed to generate Rust code with prost-build");
}
