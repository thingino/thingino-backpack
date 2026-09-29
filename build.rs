fn main() {
    // ESP-IDF's build passes the chip's name (esp32s3, esp32p4, ...) on as a cfg; the code
    // names the P4, so the cfg is declared for the builds that do not set it.
    println!("cargo::rustc-check-cfg=cfg(esp32p4)");
    embuild::espidf::sysenv::output();
}
