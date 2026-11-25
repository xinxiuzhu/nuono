//! 源码插件工厂直接随宿主编译进同一二进制程序，无动态库加载或跨进程协议边界。
use super::{LoadedPlugin, Plugin};
use std::sync::OnceLock;

pub type PluginFactory = fn() -> Box<dyn Plugin>;
static FACTORIES: OnceLock<Vec<PluginFactory>> = OnceLock::new();

/// 在启动宿主运行时之前由主程序调用一次，注册所有编译期内置的插件工厂。
pub fn register_factories(factories: Vec<PluginFactory>) -> anyhow::Result<()> {
    FACTORIES
        .set(factories)
        .map_err(|_| anyhow::anyhow!("plugin factories already registered"))
}

pub fn instantiate() -> anyhow::Result<Vec<LoadedPlugin>> {
    Ok(instantiate_registered()?.unwrap_or_default())
}

/// None 表示应用尚未声明工厂目录；Some(empty) 表示明确注册了空目录。
/// 任一构造器 panic 会使整次发现失败，不返回可被误当成完整名单的部分结果。
pub(crate) fn instantiate_registered() -> anyhow::Result<Option<Vec<LoadedPlugin>>> {
    instantiate_factories(FACTORIES.get().map(Vec::as_slice))
}

fn instantiate_factories(
    factories: Option<&[PluginFactory]>,
) -> anyhow::Result<Option<Vec<LoadedPlugin>>> {
    let Some(factories) = factories else {
        return Ok(None);
    };
    let plugins = factories
        .iter()
        .map(|factory| {
            std::panic::catch_unwind(*factory)
                .map(|plugin| LoadedPlugin { plugin })
                .map_err(|_| anyhow::anyhow!("plugin constructor panicked"))
        })
        .collect::<anyhow::Result<Vec<_>>>()?;
    Ok(Some(plugins))
}

#[cfg(test)]
mod tests {
    use super::*;

    struct FixturePlugin;

    #[async_trait::async_trait]
    impl Plugin for FixturePlugin {
        fn name(&self) -> &str {
            "FixturePlugin"
        }

        fn version(&self) -> &str {
            "1.0.0"
        }

        fn description(&self) -> &str {
            "Factory discovery fixture"
        }
    }

    fn fixture_factory() -> Box<dyn Plugin> {
        Box::new(FixturePlugin)
    }

    fn panicking_factory() -> Box<dyn Plugin> {
        panic!("fixture constructor failure")
    }

    #[test]
    fn unregistered_factories_do_not_claim_an_empty_inventory() {
        assert!(instantiate_factories(None).unwrap().is_none());
    }

    #[test]
    fn explicitly_registered_empty_factories_are_a_complete_empty_inventory() {
        assert!(
            instantiate_factories(Some(&[]))
                .unwrap()
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn registered_factories_return_the_complete_inventory() {
        let factories: [PluginFactory; 1] = [fixture_factory];
        let plugins = instantiate_factories(Some(&factories)).unwrap().unwrap();
        assert_eq!(plugins.len(), 1);
        assert_eq!(plugins[0].name(), "FixturePlugin");
    }

    #[test]
    fn constructor_panic_rejects_previously_constructed_partial_inventory() {
        let factories: [PluginFactory; 3] = [fixture_factory, panicking_factory, fixture_factory];
        assert!(instantiate_factories(Some(&factories)).is_err());
    }
}
