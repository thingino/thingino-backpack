use std::io::Write;
use std::path::Path;

fn main() {
    // ESP-IDF's build passes the chip's name (esp32s3, esp32p4, ...) on as a cfg; the code
    // names each chip it builds for, so their cfgs are declared, set in a build or not.
    println!("cargo::rustc-check-cfg=cfg(esp32s2, esp32s3, esp32p4)");
    embuild::espidf::sysenv::output();
    // The status page, gzipped: browsers take it so, and it is a third of the size.
    let page = std::fs::read("src/status.html").expect("reading src/status.html");
    let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::best());
    gz.write_all(&page).expect("gzipping the status page");
    let out = std::env::var("OUT_DIR").expect("OUT_DIR");
    std::fs::write(Path::new(&out).join("status.html.gz"), gz.finish().expect("gzipping the status page"))
        .expect("writing the gzipped status page");
}
