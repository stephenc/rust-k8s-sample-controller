extern crate k8s_openapi;
extern crate kube;
extern crate kube_derive;
#[macro_use]
extern crate log;
extern crate serde_derive;
extern crate serde_json;
extern crate serde_yaml;
extern crate tokio;

use std::collections::BTreeMap;

use chrono::Utc;
use env_logger::Env;
use futures::{StreamExt, TryStreamExt};
use k8s_openapi::api::apps::v1::{Deployment, DeploymentSpec};
use k8s_openapi::api::core::v1::{
    Container, Event, EventSource, ObjectReference, PodSpec, PodTemplateSpec,
};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::{LabelSelector, OwnerReference, Time};
use kube::{Api, config, Resource};
use kube::api::{ListParams, Meta, ObjectMeta, PostParams, WatchEvent};
use kube::client::APIClient;
use kube::runtime::Informer;
use kube_derive::CustomResource;
use serde_derive::{Deserialize, Serialize};

#[derive(CustomResource, Serialize, Deserialize, Clone, Debug)]
#[kube(group = "samplecontroller.k8s.io", version = "v1alpha1", namespaced)]
#[kube(status = "FooStatus")]
#[kube(apiextensions = "v1beta1")]
#[serde(rename_all = "camelCase")]
pub struct FooSpec {
    deployment_name: String,
    replicas: Option<i32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FooStatus {
    available_replicas: Option<i32>,
}

/// Entry point
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Set up logging
    env_logger::Builder::from_env(Env::default().default_filter_or("info")).init();

    // Load the kubeconfig file.
    let kubeconfig = config::load_kube_config().await?;

    // Create a new client
    let client = APIClient::new(kubeconfig);

    // Follow the resource in all namespaces
    let resource = Resource::all::<Foo>();

    // Create our informer and start listening
    let lp = ListParams::default();
    let ei = Informer::new(client.clone(), lp, resource);
    loop {
        let mut foos = ei.poll().await?.boxed();

        // Now we just do something each time a new Foo event is triggered
        while let Some(event) = foos.try_next().await? {
            handle(client.clone(), event).await?;
        }
    }
}

/// Event handler
async fn handle(client: APIClient, event: WatchEvent<Foo>) -> anyhow::Result<()> {
    match event {
        WatchEvent::Added(foo) => {
            debug!("Foo {}/{} added", &foo.namespace().unwrap(), &foo.name());
            sync_handler(client, foo).await
        }
        WatchEvent::Modified(foo) => {
            debug!("Foo {}/{} modified", &foo.namespace().unwrap(), &foo.name());
            sync_handler(client, foo).await
        }
        WatchEvent::Deleted(foo) => {
            debug!("Foo {}/{} deleted", &foo.namespace().unwrap(), &foo.name());
            Ok(())
        }
        _ => Ok(()),
    }
}

/// sync_handler compares the actual state with the desired, and attempts to
/// converge the two. It then updates the Status block of the Foo resource
/// with the current status of the resource.
async fn sync_handler(client: APIClient, foo: Foo) -> anyhow::Result<()> {
    let pp = PostParams::default();
    let foo_name = foo.name();
    let namespace = foo.namespace().unwrap();
    let deployment_name = foo.spec.deployment_name.clone();
    if deployment_name.is_empty() {
        // We choose to absorb the error here as the worker would requeue the
        // resource otherwise. Instead, the next time the resource is updated
        // the resource will be queued again.
        error!(
            "{}/{}: deployment name must be specified",
            namespace, &foo_name
        );
        return Ok(());
    }
    // Get the deployment with the name specified in Foo.spec
    let deployments: Api<Deployment> = Api::namespaced(client.clone(), &namespace);
    let deployment = match deployments.get(&deployment_name).await {
        Ok(d) => d,
        Err(_) => {
            // If the resource doesn't exist, we'll create it
            let rsp = deployments.create(&pp, serde_json::to_vec(&new_deployment(&foo))?);

            // If an error occurs during Get / Create, we'll requeue the item so we can
            // attempt processing again later. This could have been caused by a
            // temporary network failure, or any other transient reason.
            rsp.await?
        }
    };
    // If the Deployment is not controlled by this Foo resource, we should log
    // a warning to the event recorder and return error msg.
    if let Some(owners) = deployment.metadata.unwrap().owner_references {
        if !owners.iter().any(|owner| {
            owner.controller == Some(true) && Some(owner.uid.clone()) == foo.meta().uid
        }) {
            return record_event(
                client.clone(),
                &foo,
                "ErrResourceExists",
                &format!(
                    "Resource {}/{} already exists and is not managed by Foo",
                    namespace, deployment_name
                ),
                "Normal",
            )
                .await;
        }
    }
    // If this number of the replicas on the Foo resource is specified, and the
    // number does not equal the current desired replicas on the Deployment, we
    // should update the Deployment resource.
    let foo_replicas = foo.spec.replicas.unwrap_or(1);
    let deployment_replicas = deployment.spec.clone().unwrap().replicas.unwrap_or(1);
    if foo_replicas != deployment_replicas {
        info!(
            "Foo {}/{} replicas: {}, deployment replicas: {}",
            namespace, deployment_name, foo_replicas, deployment_replicas
        );
        let rsp = deployments.replace(
            &deployment_name,
            &pp,
            serde_json::to_vec(&new_deployment(&foo))?,
        );
        // If an error occurs during Update, we'll requeue the item so we can
        // attempt processing again later. This could have been caused by a
        // temporary network failure, or any other transient reason.
        rsp.await.map(|_| ()).map_err(|e| anyhow::Error::new(e))?
    }

    // Finally, we update the status block of the Foo resource to reflect the
    // current state of the world
    let new_foo = Foo {
        status: Some(FooStatus {
            available_replicas: deployment.status.unwrap().available_replicas.clone(),
        }),
        ..foo.clone()
    };
    // If the CustomResourceSubresources feature gate is not enabled,
    // we must use Update instead of UpdateStatus to update the Status block of the Foo resource.
    // UpdateStatus will not allow changes to the Spec of the resource,
    // which is ideal for ensuring nothing other than resource status has been updated.
    Api::<Foo>::namespaced(client.clone(), &namespace)
        .replace(&foo_name, &pp, serde_json::to_vec(&new_foo).unwrap())
        .await?;

    record_event(
        client.clone(),
        &foo,
        "Synced",
        "Foo synced successfully",
        "Normal",
    )
        .await
}

/// new_deployment creates a new Deployment for a Foo resource. It also sets
/// the appropriate OwnerReferences on the resource so sync_handler can discover
/// the Foo resource that 'owns' it.
fn new_deployment(foo: &Foo) -> Deployment {
    let mut labels = BTreeMap::<String, String>::new();
    labels.insert("app".into(), "nginx".into());
    labels.insert("controller".into(), foo.name().into());
    let labels = Some(labels);
    Deployment {
        metadata: Some(ObjectMeta {
            name: Some(foo.spec.deployment_name.clone()),
            namespace: foo.namespace(),
            owner_references: Some(vec![OwnerReference {
                api_version: k8s_openapi::api_version(foo).to_string(),
                kind: k8s_openapi::kind(foo).to_string(),
                controller: Some(true),
                name: foo.name(),
                uid: foo.meta().uid.clone().unwrap(),
                ..Default::default()
            }]),
            ..Default::default()
        }),
        spec: Some(DeploymentSpec {
            replicas: Some(foo.spec.replicas.unwrap_or(1)),
            selector: LabelSelector {
                match_labels: labels.clone(),
                ..Default::default()
            },
            template: PodTemplateSpec {
                metadata: Some(ObjectMeta {
                    labels: labels.clone(),
                    ..Default::default()
                }),
                spec: Some(PodSpec {
                    containers: vec![Container {
                        image: Some("nginx:latest".to_string()),
                        name: "nginx".to_string(),
                        ..Default::default()
                    }],
                    ..Default::default()
                }),
            },
            ..Default::default()
        }),
        ..Default::default()
    }
}

// // // // Things that are missing from the kube crate // // // //

/// Creates and records an event against the supplied object
async fn record_event<T>(
    client: APIClient,
    obj: &T,
    reason: &str,
    message: &str,
    type_: &str,
) -> anyhow::Result<()>
    where
        T: Meta,
{
    let ref_ = to_object_reference(obj);
    let t = Utc::now();
    let namespace = match &ref_.namespace {
        Some(namespace) => {
            if namespace.is_empty() {
                "default".to_string()
            } else {
                namespace.to_string()
            }
        }
        None => "default".to_string(),
    };
    let ref_name: String = ref_.name.as_ref().unwrap().to_string();
    let event = Event {
        metadata: ObjectMeta {
            name: Some(format!("{}.{}", ref_name, t.timestamp_nanos())),
            namespace: Some(namespace.clone()),
            ..Default::default()
        },
        involved_object: ref_.clone(),
        reason: Some(reason.to_string()),
        message: Some(message.to_string()),
        first_timestamp: Some(Time(t)),
        last_timestamp: Some(Time(t)),
        count: Some(1),
        type_: Some(type_.to_string()),
        source: Some(EventSource {
            component: Some("sample-controller".to_string()),
            ..Default::default()
        }),
        ..Default::default()
    };
    let pp = PostParams::default();
    Api::<Event>::namespaced(client.clone(), &namespace)
        .create(&pp, serde_json::to_vec(&event)?)
        .await
        .map(|_| ())
        .map_err(|e| anyhow::Error::new(e))
}

/// Creates the ObjectReference of a type.
fn to_object_reference<T>(foo: &T) -> ObjectReference
    where
        T: Meta,
{
    ObjectReference {
        api_version: Some(k8s_openapi::api_version(foo).to_string()),
        kind: Some(k8s_openapi::kind(foo).to_string()),
        name: Some(foo.name()),
        namespace: foo.namespace(),
        uid: Some(foo.meta().uid.clone().unwrap()),
        resource_version: foo.meta().resource_version.clone(),
        ..Default::default()
    }
}

