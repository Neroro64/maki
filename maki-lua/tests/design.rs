use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use maki_agent::prompt::{PromptId, Slot, assemble};
use maki_agent::tools::ToolRegistry;
use maki_config::PluginsConfig;
use maki_lua::test_support::{runtime, wait_next};
use maki_lua::{PluginHost, UiAction};

const CUSTOM_IDENTITY: &str = "Keep the user's custom identity.";
const CUSTOM_TONE: &str = "Keep the user's custom tone.";
const COMMAND_TIMEOUT: Duration = Duration::from_secs(5);

#[test]
fn design_toggle_adds_dynamic_conventions_without_replacing_identity_or_tone() {
    let mut host = PluginHost::new(Arc::new(ToolRegistry::new())).unwrap();
    host.load_source(
        "custom_prompt",
        &format!(
            r#"
            maki.api.set_prompt({{ slot = "identity", content = "{CUSTOM_IDENTITY}" }})
            maki.api.set_prompt({{ slot = "tone", content = "{CUSTOM_TONE}" }})
            "#
        ),
    )
    .unwrap();
    host.load_builtins(&PluginsConfig {
        enabled: true,
        names: vec!["design".to_owned()],
        packages: Vec::new(),
        opts: HashMap::new(),
    })
    .unwrap();
    let events = host.event_handle();
    let mut ui = host.take_ui_action_rx().unwrap();
    let before = runtime().block_on(events.collect_prompt_slots(None));
    let original = assemble(PromptId::System, &before, "");
    assert!(before.get(PromptId::System, Slot::Conventions).is_empty());

    for (argument, active) in [
        ("on", true),
        ("off", false),
        ("toggle", true),
        ("toggle", false),
        ("invalid", false),
    ] {
        events.run_command(
            Arc::from("design"),
            Arc::from("/design"),
            argument.to_owned(),
            0,
        );
        loop {
            if matches!(
                wait_next(&mut ui, COMMAND_TIMEOUT).unwrap(),
                UiAction::Flash(_)
            ) {
                break;
            }
        }
        let slots = runtime().block_on(events.collect_prompt_slots(None));
        assert_eq!(
            slots.get(PromptId::System, Slot::Identity)[0].content,
            CUSTOM_IDENTITY
        );
        assert_eq!(
            slots.get(PromptId::System, Slot::Tone)[0].content,
            CUSTOM_TONE
        );
        let conventions = slots.get(PromptId::System, Slot::Conventions);
        assert_eq!(!conventions.is_empty(), active);
        if active {
            assert!(conventions[0].content.contains("view_image"));
            assert!(conventions[0].content.contains("computer"));
        } else {
            assert_eq!(assemble(PromptId::System, &slots, ""), original);
        }
    }
}
