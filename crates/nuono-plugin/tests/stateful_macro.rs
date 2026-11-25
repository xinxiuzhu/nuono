use nuono_plugin::prelude::*;
use std::sync::Arc;

#[derive(Default)]
struct CommandState {
    label: String,
}

#[derive(Default)]
#[nuono_plugin(
    name = "StatefulPlugin",
    version = "0.1.0",
    description = "stateful macro test"
)]
struct StatefulPlugin {
    state: Arc<CommandState>,
}

#[nuono_command(pattern = "stateful")]
async fn stateful(
    _bot: &Bot,
    _event: &Message,
    _data: MatchData,
    state: &CommandState,
) -> Result<()> {
    let _ = &state.label;
    Ok(())
}

#[test]
fn stateful_command_compiles_and_collects_with_only_sdk_paths() {
    let plugin = StatefulPlugin::default();

    let commands = plugin.commands();
    assert_eq!(commands.len(), 1);
    assert_eq!(commands[0].name, "stateful");
}
