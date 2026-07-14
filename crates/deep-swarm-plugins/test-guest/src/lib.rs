wit_bindgen::generate!({
    path: "../wit",
    world: "plugin-world",
    generate_all,
});

struct TestPlugin;

impl exports::deepswarm::plugin::plugin::Guest for TestPlugin {
    fn init(config_json: String) -> Result<String, String> {
        deepswarm::plugin::host::log("info", "test plugin initialized");
        Ok(config_json)
    }

    fn on_event(value: exports::deepswarm::plugin::plugin::Event) -> Result<String, String> {
        Ok(value.payload_json)
    }

    fn shutdown() -> Result<String, String> {
        Ok("shutdown".into())
    }
}

export!(TestPlugin);
