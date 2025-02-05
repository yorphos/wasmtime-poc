use std::{collections::HashMap, path::Path, sync::Arc};

use serde::Deserialize;
use tokio::sync::mpsc;
use wasmtime::{Engine, Linker, Module, Store};

use crate::{
    debug_api,
    module::{
        initialize_mqtt_for_module, mqtt_event_loop_task, ModuleConfig, ModuleRuntimeConfig,
        WasmModuleStore,
    },
    mqtt_api,
};

#[derive(Debug)]
pub enum RuntimeEvent {
    RuntimeTaskStop,
}

#[derive(Deserialize)]
pub struct AppConfig {
    pub modules: HashMap<String, ModuleConfig>,
}

pub struct UninitializedModule<C> {
    bytes: Box<[u8]>,
    runtime_config: C,
}

#[derive(Clone)]
pub struct InitializedModule<T, C> {
    pub module: Module,
    pub linker: Linker<T>,
    pub engine: Arc<Engine>,
    pub runtime_config: C,
}

pub struct UninitializedAppContext {
    modules: HashMap<String, UninitializedModule<ModuleRuntimeConfig>>,
}

struct MqttEventLoopTaskInfo {
    pub runtime_event_sender: tokio::sync::mpsc::Sender<RuntimeEvent>,
    pub task_handle: tokio::task::JoinHandle<anyhow::Result<()>>,
}

struct ModuleRuntime {
    module_task_handle: tokio::task::JoinHandle<Result<(), wasmtime::Trap>>,
    module_mqtt_event_loop_task_info: Option<MqttEventLoopTaskInfo>,
}

struct ModuleData {
    module_template: InitializedModule<WasmModuleStore, ModuleRuntimeConfig>,
    runtime: Option<ModuleRuntime>,
}

pub struct InitializedAppContext {
    modules: HashMap<String, ModuleData>,
}

impl AppConfig {
    pub fn from_app_config_file(path: impl AsRef<Path>) -> anyhow::Result<AppConfig> {
        let config_file_contents = std::fs::read_to_string(path)?;

        Ok(toml::from_str(&config_file_contents)?)
    }
}

impl UninitializedAppContext {
    pub fn new(config: &AppConfig) -> anyhow::Result<UninitializedAppContext> {
        let modules: Result<HashMap<String, UninitializedModule<ModuleRuntimeConfig>>, _> =
            config
                .modules
                .iter()
                .map(
                    |(module_name, module_config)| -> std::io::Result<(String, UninitializedModule<ModuleRuntimeConfig>)> {
                        Ok((
                            module_name.clone(),
                            UninitializedModule::<ModuleRuntimeConfig> {
                                bytes: std::fs::read(&module_config.wasm_module_path)?
                                    .into_boxed_slice(),
                                runtime_config: module_config.runtime.clone(),
                            },
                        ))
                    },
                )
                .collect();

        Ok(UninitializedAppContext { modules: modules? })
    }

    pub fn initialize_modules(self) -> anyhow::Result<InitializedAppContext> {
        let engine = Arc::new(Engine::default());

        let initialized_modules: Result<HashMap<String, ModuleData>, _> = self
            .modules
            .into_iter()
            .map(
                |(module_name, module)| -> anyhow::Result<(String, ModuleData)> {
                    let mut linker = Linker::<WasmModuleStore>::new(&engine);

                    let compiled_module = Module::from_binary(&engine, &module.bytes)?;

                    mqtt_api::add_to_linker(&mut linker, |s| &mut s.mqtt_connection)?;
                    debug_api::add_to_linker(&mut linker, |s| s)?;

                    Ok((
                        module_name,
                        ModuleData {
                            module_template: InitializedModule::<
                                WasmModuleStore,
                                ModuleRuntimeConfig,
                            > {
                                module: compiled_module,
                                linker,
                                engine: engine.clone(),
                                runtime_config: module.runtime_config,
                            },
                            runtime: None,
                        },
                    ))
                },
            )
            .collect();

        Ok(InitializedAppContext {
            modules: initialized_modules?,
        })
    }
}

impl InitializedAppContext {
    pub async fn cleanup_finished_modules(
        &mut self,
    ) -> anyhow::Result<Vec<Result<(), wasmtime::Trap>>> {
        let mut results = vec![];

        for (_module_name, module_data) in self.modules.iter_mut() {
            if let Some(runtime) = &mut module_data.runtime {
                if runtime.module_task_handle.is_finished() {
                    let runtime = module_data
                        .runtime
                        .take()
                        .expect("runtime presence was checked above");

                    if let Some(mqtt_event_loop_task_info) =
                        runtime.module_mqtt_event_loop_task_info
                    {
                        mqtt_event_loop_task_info
                            .runtime_event_sender
                            .send(RuntimeEvent::RuntimeTaskStop)
                            .await?;

                        if let Err(e) = mqtt_event_loop_task_info.task_handle.await? {
                            eprintln!("MQTT event loop task error: {}", e);
                        }
                    }

                    results.push(runtime.module_task_handle.await?);
                }
            }
        }

        Ok(results)
    }

    pub fn run_all_modules(&mut self) -> anyhow::Result<()> {
        for (module_name, module_data) in self.modules.iter_mut() {
            if let None = module_data.runtime {
                let module_template = &mut module_data.module_template;
                let mut mqtt_connection = None;
                let mut module_mqtt_event_loop_task_info = None;

                if let Some(mqtt_runtime) =
                    initialize_mqtt_for_module(&module_template.runtime_config)
                {
                    match mqtt_runtime {
                        Ok(mqtt_runtime) => {
                            mqtt_connection = Some(mqtt_runtime.mqtt);

                            let (mqtt_event_loop_runtime_sender, mqtt_event_loop_runtime_receiver) =
                                mpsc::channel(32);

                            let mqtt_event_loop_task_handle = tokio::spawn(async move {
                                mqtt_event_loop_task(
                                    mqtt_runtime.event_channel_sender,
                                    mqtt_event_loop_runtime_receiver,
                                    mqtt_runtime.event_loop,
                                )
                                .await
                            });

                            let mqtt_event_loop_task_info = MqttEventLoopTaskInfo {
                                runtime_event_sender: mqtt_event_loop_runtime_sender,
                                task_handle: mqtt_event_loop_task_handle,
                            };

                            module_mqtt_event_loop_task_info = Some(mqtt_event_loop_task_info);
                        }
                        Err(e) => eprintln!(
                            "Error starting MQTT runtime for module '{}': {}",
                            module_name, e
                        ),
                    }
                }

                let mut store =
                    Store::new(&module_template.engine, WasmModuleStore { mqtt_connection });
                let instance = module_template
                    .linker
                    .instantiate(&mut store, &module_template.module)?;
                let wasm_entrypoint = instance.get_typed_func::<(), (), _>(&mut store, "start")?;

                let module_task_handle =
                    tokio::task::spawn_blocking(move || wasm_entrypoint.call(&mut store, ()));

                let module_runtime = ModuleRuntime {
                    module_task_handle,
                    module_mqtt_event_loop_task_info,
                };

                module_data.runtime = Some(module_runtime);
            }
        }

        Ok(())
    }
}
