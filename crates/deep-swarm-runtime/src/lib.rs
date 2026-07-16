pub fn workspace_components() -> [&'static str; 4] {
    [
        deep_swarm_deepseek::crate_name(),
        deep_swarm_storage::crate_name(),
        deep_swarm_tools::crate_name(),
        deep_swarm_tokenizer::crate_name(),
    ]
}
