use anyhow::bail;
use anyhow::Context;
use anyhow::Result;
use futures::StreamExt;
use futures::TryStreamExt;
use k8s_openapi::api::core::v1::LoadBalancerIngress;
use k8s_openapi::api::core::v1::LoadBalancerStatus;
use k8s_openapi::api::core::v1::ServiceStatus;
use k8s_openapi::{
    api::{
        apps::v1::{Deployment, DeploymentSpec, DeploymentStrategy},
        core::v1::{
            Capabilities, Container, EnvVar, PersistentVolumeClaim, PersistentVolumeClaimSpec,
            PersistentVolumeClaimVolumeSource, Pod, PodSpec, PodTemplateSpec, ResourceRequirements,
            SecurityContext, Service, ServiceSpec, Volume, VolumeMount, VolumeResourceRequirements,
        },
    },
    apimachinery::pkg::{
        api::resource::Quantity,
        apis::meta::v1::{LabelSelector, OwnerReference},
    },
};
use kube::runtime::finalizer;
use kube::{
    api::{Api, AttachParams, ObjectMeta, Patch, PatchParams, WatchParams},
    core::WatchEvent,
    runtime::{
        controller::{Action, Config, Controller},
        watcher,
    },
    Client, Resource, ResourceExt,
};
use reqwest::header;
use serde::Deserialize;
use std::env;
use std::env::VarError;
use std::error::Error;
use std::fmt::Write;
use std::{collections::BTreeMap, sync::Arc};
use tokio::time::Duration;
use tracing::*;

use crate::context::Data;

mod api;
mod context;
mod error;

#[derive(Deserialize)]
struct LocalStatusSelf {
    #[serde(rename = "PublicKey")]
    node_key: String,
}

#[derive(Deserialize)]
struct LocalStatus {
    #[serde(rename = "BackendState")]
    backend_state: String,

    #[serde(rename = "AuthURL")]
    auth_url: String,

    #[serde(rename = "Self")]
    this_node: LocalStatusSelf,
}
async fn get_pod_metadata(
    client: Client,
    namespace: &str,
    service: &String,
) -> anyhow::Result<LocalStatus> {
    let pod_api = Api::<Pod>::namespaced(client.clone(), namespace);

    // Find pod and wait for it to be read
    let wp = WatchParams::default()
        .labels(&format!("surge.unrouted.uk/service={service}"))
        .timeout(60);
    let mut stream = pod_api.watch(&wp, "0").await?.boxed();
    while let Some(status) = stream.try_next().await.unwrap() {
        match status {
            WatchEvent::Modified(o) | WatchEvent::Added(o) => {
                let s = o.status.as_ref().expect("status exists on pod");
                if s.phase.clone().unwrap_or_default() == "Running" {
                    let name = o.name_unchecked();

                    info!("Ready to attach to {}", o.name_any());
                    // Get status
                    let ap = AttachParams::default()
                        .container("tailscale")
                        .stdin(false)
                        .stderr(false);
                    let mut attached = match pod_api
                        .exec(
                            &name,
                            vec![
                                "tailscale",
                                "--socket",
                                "/tmp/tailscaled.sock",
                                "status",
                                "--json",
                            ],
                            &ap,
                        )
                        .await
                    {
                        Ok(attached) => attached,
                        Err(e) => return Err(e.into()),
                    };
                    let stdout = tokio_util::io::ReaderStream::new(attached.stdout().unwrap());
                    let out = stdout
                        .filter_map(|r| async {
                            r.ok().and_then(|v| String::from_utf8(v.to_vec()).ok())
                        })
                        .collect::<Vec<_>>()
                        .await
                        .join("");
                    attached.join().await?;

                    // tln!("{}", out);

                    let payload: LocalStatus = match serde_json::from_str(&out) {
                        Ok(result) => result,
                        Err(e) => return Err(e.into()),
                    };

                    return Ok(payload);
                }
            }
            _ => {}
        }
    }

    bail!("Timeout")
}

async fn ensure_node_registered(
    ctx: &Data,
    service: &Service,
    user: &str,
) -> anyhow::Result<api::Node> {
    if let Some(annotations) = &service.meta().annotations {
        if let Some(nodekey) = annotations.get("surge.unrouted.uk/nodekey") {
            return match api::get_node_by_nodekey(ctx, nodekey).await? {
                Some(machine) => Ok(machine),
                None => bail!("Could not find node '{}' in control plane, but thats what tunnel is annotated as", nodekey)
            };
        }
    }

    let namespace = service.meta().namespace.as_ref().unwrap();
    let name = service.meta().name.as_ref().unwrap();

    let payload = get_pod_metadata(ctx.client.clone(), namespace, name).await?;

    return Ok(match payload.backend_state.as_str() {
        "NeedsLogin" => {
            let auth_url = url::Url::parse(&payload.auth_url).context("Invalid AuthURL")?;
            let nodekey = auth_url
                .path_segments()
                .context("AuthURL has no path segments")?
                .last()
                .unwrap()
                .split_once(':')
                .unwrap()
                .1;

            // We check this nodekey isn't already registed
            // This is just belts and braces to prevent calling register multiple times for the same nodekey
            match api::get_node_by_nodekey(ctx, nodekey).await? {
                Some(machine) => machine,
                None => api::register_node(ctx, nodekey, user).await?,
            }
        }
        "Running" => {
            let nodekey = &payload.this_node.node_key.split_once(':').unwrap().1;
            match api::get_node_by_nodekey(ctx, nodekey).await? {
                        Some(machine) => machine,
                        None => bail!("Could not find node '{}' in control plane, but node is in 'Running' state. Bailing for now", nodekey)
                    }
        }
        state => bail!("Unknown node state: {}", state),
    });
}

async fn apply(service: Arc<Service>, ctx: Arc<Data>) -> anyhow::Result<Action> {
    let client = &ctx.client;

    let spec = service.spec.as_ref().context("Missing object key .spec")?;

    if spec.load_balancer_class != Some(ctx.load_balancer_class.clone()) {
        return Ok(Action::requeue(Duration::from_secs(300)));
    }

    let name = service
        .meta()
        .name
        .as_ref()
        .context("Missing object key .metadata.name")?;
    let namespace = service
        .meta()
        .namespace
        .as_ref()
        .context("Missing object key .metadata.namespace")?;
    let uid = service
        .meta()
        .uid
        .as_ref()
        .context("Missing object key .spec")?;
    let cluster_ip = spec
        .cluster_ip
        .as_ref()
        .context("Missing object key .spec.clusterIP")?;

    let default_user = ctx.default_user.clone();
    let user = match service.meta().labels.as_ref() {
        Some(labels) => labels
            .get("surge.unrouted.uk/user")
            .or(default_user.as_ref()),
        None => default_user.as_ref(),
    }
    .context("Must set surge.unrouted.uk/user")?
    .clone();

    let hostname = match service.meta().labels.as_ref() {
        Some(labels) => labels.get("surge.unrouted.uk/hostname").or(Some(name)),
        None => Some(name),
    }
    .unwrap()
    .clone();

    let dns_name = match service.meta().labels.as_ref() {
        Some(labels) => labels.get("surge.unrouted.uk/dnsname").or(Some(&hostname)),
        None => Some(&hostname),
    }
    .unwrap()
    .clone();

    let owner_reference = OwnerReference {
        api_version: Service::api_version(&()).to_string(),
        kind: Service::kind(&()).to_string(),
        name: name.clone(),
        uid: uid.clone(),
        ..Default::default()
    };

    let pvc_name = format!("{name}-tailscale-state");

    let pvc = PersistentVolumeClaim {
        metadata: ObjectMeta {
            name: Some(pvc_name.clone()),
            namespace: service.meta().namespace.clone(),
            owner_references: Some(vec![owner_reference.clone()]),
            labels: Some(BTreeMap::from([(
                "surge.unrouted.uk/service".to_string(),
                name.clone(),
            )])),
            ..Default::default()
        },
        spec: Some(PersistentVolumeClaimSpec {
            storage_class_name: Some(ctx.storage_class.clone()),
            access_modes: Some(vec!["ReadWriteOnce".to_string()]),
            resources: Some(VolumeResourceRequirements {
                requests: Some(BTreeMap::from([(
                    "storage".to_string(),
                    Quantity("50Mi".to_string()),
                )])),
                ..Default::default()
            }),
            ..Default::default()
        }),
        ..Default::default()
    };

    let pvc_api = Api::<PersistentVolumeClaim>::namespaced(client.clone(), namespace);
    pvc_api
        .patch(
            pvc.metadata
                .name
                .as_ref()
                .context("Missing object key .metadata.name")?,
            &PatchParams::apply("surge.unrouted.uk"),
            &Patch::Apply(&pvc),
        )
        .await?;

    info!("PVC updated");

    let deployment = Deployment {
        metadata: ObjectMeta {
            name: Some(format!("{name}-tailscale-proxy")),
            namespace: service.meta().namespace.clone(),
            owner_references: Some(vec![owner_reference.clone()]),
            labels: Some(BTreeMap::from([(
                "surge.unrouted.uk/service".to_string(),
                name.clone(),
            )])),
            ..Default::default()
        },
        spec: Some(DeploymentSpec {
            strategy: Some(DeploymentStrategy {
                type_: Some("Recreate".to_string()),
                rolling_update: None,
            }),
            selector: LabelSelector {
                match_labels: Some(BTreeMap::from([(
                    "surge.unrouted.uk/service".to_string(),
                    name.clone(),
                )])),
                ..Default::default()
            },
            template: PodTemplateSpec {
                metadata: Some(ObjectMeta {
                    labels: Some(BTreeMap::from([(
                        "surge.unrouted.uk/service".to_string(),
                        name.clone(),
                    )])),
                    ..Default::default()
                }),
                spec: Some(PodSpec {
                    volumes: Some(vec![Volume {
                        name: "tailscale-state".to_string(),
                        persistent_volume_claim: Some(PersistentVolumeClaimVolumeSource {
                            claim_name: pvc_name,
                            read_only: Some(false),
                        }),
                        ..Default::default()
                    }]),
                    init_containers: Some(vec![Container {
                        name: "setup-forwarding".to_string(),
                        image: Some("busybox".to_string()),
                        command: Some(vec!["/bin/sh".to_string()]),
                        args: Some(vec![
                            "-c".to_string(),
                            "sysctl -w net.ipv4.ip_forward=1 net.ipv6.conf.all.forwarding=1"
                                .to_string(),
                        ]),
                        security_context: Some(SecurityContext {
                            privileged: Some(true),
                            ..Default::default()
                        }),
                        resources: Some(ResourceRequirements {
                            requests: Some(BTreeMap::from([
                                ("cpu".to_string(), Quantity("1m".to_string())),
                                ("memory".to_string(), Quantity("1Mi".to_string())),
                            ])),
                            ..Default::default()
                        }),
                        ..Default::default()
                    }]),
                    containers: vec![Container {
                        name: "tailscale".to_string(),
                        image: Some(ctx.tailscale_image.clone()),
                        env: Some(vec![
                            EnvVar {
                                name: "TS_STATE_DIR".to_string(),
                                value: Some("/var/lib/tailscale".to_string()),
                                ..Default::default()
                            },
                            EnvVar {
                                name: "TS_USERSPACE".to_string(),
                                value: Some("false".to_string()),
                                ..Default::default()
                            },
                            EnvVar {
                                name: "TS_AUTH_ONCE".to_string(),
                                value: Some("true".to_string()),
                                ..Default::default()
                            },
                            EnvVar {
                                name: "TS_KUBE_SECRET".to_string(),
                                ..Default::default()
                            },
                            // These need to come from service
                            EnvVar {
                                name: "TS_HOSTNAME".to_string(),
                                value: Some(hostname.clone()),
                                ..Default::default()
                            },
                            EnvVar {
                                name: "TS_DEST_IP".to_string(),
                                value: Some(cluster_ip.clone()),
                                ..Default::default()
                            },
                            // These need injecting in
                            EnvVar {
                                name: "TS_EXTRA_ARGS".to_string(),
                                value: Some(format!("--login-server={}", ctx.base_url).to_string()),
                                ..Default::default()
                            },
                        ]),
                        volume_mounts: Some(vec![VolumeMount {
                            name: "tailscale-state".to_string(),
                            mount_path: "/var/lib/tailscale".to_string(),
                            ..Default::default()
                        }]),
                        security_context: Some(SecurityContext {
                            capabilities: Some(Capabilities {
                                add: Some(vec!["NET_ADMIN".to_string()]),
                                ..Default::default()
                            }),
                            ..Default::default()
                        }),
                        ..Default::default()
                    }],
                    ..Default::default()
                }),
            },
            ..Default::default()
        }),
        ..Default::default()
    };

    let deployment_api = Api::<Deployment>::namespaced(client.clone(), namespace);
    deployment_api
        .patch(
            deployment
                .metadata
                .name
                .as_ref()
                .context("Missing object key .metadata.name")?,
            &PatchParams::apply("surge.unrouted.uk"),
            &Patch::Apply(&deployment),
        )
        .await?;

    info!("Deployment updated");

    let node = ensure_node_registered(&ctx, &service, &user).await?;

    if node.name != dns_name {
        info!("Changing name from {} to {dns_name}", node.name);
        api::rename(&ctx, &node.id, &dns_name).await?;
    };

    if node.user.name != user {
        info!("Changing user from {} to {user}", node.user.name);
        api::change_user(&ctx, &node.id, &user).await?;
    };

    let addresses = &node.ip_addresses;
    let service = Service {
        metadata: ObjectMeta {
            namespace: service.meta().namespace.clone(),
            name: service.meta().name.clone(),
            annotations: Some(BTreeMap::from([
                ("surge.unrouted.uk/nodekey".to_string(), node.node_key),
                ("surge.unrouted.uk/machine-id".to_string(), node.id),
            ])),
            ..Default::default()
        },
        spec: Some(ServiceSpec {
            load_balancer_ip: addresses.last().cloned(),
            ..Default::default()
        }),
        ..Default::default()
    };

    let service_status = Service {
        metadata: ObjectMeta {
            namespace: service.meta().namespace.clone(),
            name: service.meta().name.clone(),
            ..Default::default()
        },
        status: Some(ServiceStatus {
            load_balancer: Some(LoadBalancerStatus {
                ingress: Some(
                    addresses
                        .iter()
                        .map(|address| LoadBalancerIngress {
                            ip: Some(address.clone()),
                            ..Default::default()
                        })
                        .collect(),
                ),
            }),
            ..Default::default()
        }),
        ..Default::default()
    };
    let service_api = Api::<Service>::namespaced(client.clone(), namespace);
    service_api
        .patch(
            service
                .metadata
                .name
                .as_ref()
                .context("Missing object key .metadata.name")?,
            &PatchParams::apply("surge.unrouted.uk").force(),
            &Patch::Apply(&service),
        )
        .await?;
    service_api
        .patch_status(
            service_status
                .metadata
                .name
                .as_ref()
                .context("Missing object key .metadata.name")?,
            &PatchParams::apply("surge.unrouted.uk").force(),
            &Patch::Apply(&service_status),
        )
        .await?;

    Ok(Action::requeue(Duration::from_secs(300)))
}

async fn cleanup(service: Arc<Service>, ctx: Arc<Data>) -> Result<Action> {
    if let Some(annotations) = &service.meta().annotations {
        if let Some(id) = annotations.get("surge.unrouted.uk/machine-id") {
            api::remove_node(&ctx, id).await?;
        } else {
            error!("machine-id label missing; stale node may have been left in control plane");
        }
    }

    Ok(Action::await_change())
}

async fn reconciler(
    service: Arc<Service>,
    ctx: Arc<Data>,
) -> std::result::Result<Action, finalizer::Error<error::Error>> {
    let ns = service.meta().namespace.as_deref().unwrap();
    let services: Api<Service> = Api::namespaced(ctx.client.clone(), ns);
    finalizer(
        &services,
        "surge.unrouted.uk/node-cleanup",
        service,
        |event| async {
            match event {
                finalizer::Event::Apply(service) => Ok(apply(service, ctx).await?),
                finalizer::Event::Cleanup(service) => Ok(cleanup(service, ctx).await?),
            }
        },
    )
    .await
}

/// The controller triggers this on reconcile errors
fn error_policy(
    _object: Arc<Service>,
    _error: &finalizer::Error<error::Error>,
    _ctx: Arc<Data>,
) -> Action {
    Action::requeue(Duration::from_secs(1))
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();

    let base_url = env::var("SURGE_CONTROL_PLANE_URL")?;
    let auth_token = format!("Bearer {}", env::var("SURGE_CONTROL_PLANE_AUTH")?);
    let storage_class = env::var("SURGE_STORAGE_CLASS")?;

    let default_user = match env::var("SURGE_DEFAULT_USER") {
        Ok(username) => Some(username),
        Err(VarError::NotPresent) => None,
        Err(VarError::NotUnicode(_)) => bail!("SURGE_DEFAULT_USER is set but invalid"),
    };

    let load_balancer_class = match env::var("SURGE_LOAD_BALANCER_CLASS") {
        Ok(username) => username,
        Err(VarError::NotPresent) => "headscale".to_string(),
        Err(VarError::NotUnicode(_)) => bail!("SURGE_LOAD_BALANCER_CLASS is set but invalid"),
    };

    let tailscale_image = match env::var("SURGE_TAILSCALE_IMAGE") {
        Ok(tailscale_image) => tailscale_image,
        Err(VarError::NotPresent) => "tailscale/tailscale:stable".to_string(),
        Err(VarError::NotUnicode(_)) => bail!("SURGE_TAILSCALE_IMAGE is set but invalid"),
    };

    let mut headers = header::HeaderMap::new();
    let mut auth_value = header::HeaderValue::from_str(&auth_token)?;
    auth_value.set_sensitive(true);
    headers.insert(header::AUTHORIZATION, auth_value);

    headers.insert(
        "Accept",
        header::HeaderValue::from_static("application/json"),
    );

    let headscale = reqwest::Client::builder()
        .default_headers(headers)
        .build()?;

    let client = Client::try_default().await?;

    let services = Api::<Service>::all(client.clone());
    let pvcs = Api::<PersistentVolumeClaim>::all(client.clone());
    let deployments = Api::<Deployment>::all(client.clone());

    // limit the controller to running a maximum of two concurrent reconciliations
    let config = Config::default()
        .concurrency(2)
        .debounce(Duration::from_secs(5));

    Controller::new(services, watcher::Config::default())
        .owns(pvcs, watcher::Config::default())
        .owns(deployments, watcher::Config::default())
        .with_config(config)
        .shutdown_on_signal()
        .run(
            reconciler,
            error_policy,
            Arc::new(Data {
                client,
                base_url,
                headscale,
                default_user,
                load_balancer_class,
                storage_class,
                tailscale_image,
            }),
        )
        .for_each(|res| async move {
            match res {
                Ok(o) => info!("reconciled {:?}", o),
                Err(err) => {
                    let mut msg = err.to_string();
                    let mut source = err.source();
                    while let Some(src) = source {
                        writeln!(msg, ": {src}").unwrap();
                        source = src.source();
                    }
                    error!("reconcile failed: {}", msg);
                }
            }
        })
        .await;
    info!("controller terminated");
    Ok(())
}
