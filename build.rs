fn main() {
    // ESP-IDF's build passes the chip's name (esp32s3, esp32p4, ...) on as a cfg; the code
    // names each chip it builds for, so their cfgs are declared, set in a build or not.
    println!("cargo::rustc-check-cfg=cfg(esp32s2, esp32s3, esp32p4)");
    embuild::espidf::sysenv::output();
}
