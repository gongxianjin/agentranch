//! Validate Helper output against Manager context and installed trust before Store I/O.
use anyhow::{Context, Result, ensure};
use serde::Deserialize;
use std::{io::Read, path::{Component, Path, PathBuf}};
use crate::{config::{Config, Mode}, adapters::model_endpoint::ModelEndpoint};
use super::{native_worker::{NativeWorkerConfig, CgroupRootMode, digest_file}, provision::{ProvisionResult, ProvisioningContext, ensure_no_reparse_ancestors}, service::ProvisionRequest, ssh_gateway::{SshGatewayConfig, GatewayLaunchMode}};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Prerequisites { schema:String, asset_manifest:String, asset_version:String, known_hosts:String, tls_certificate:String }
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Assets { schema:String, version:String, worker:Asset, loader:Asset }
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Asset { path:String, bytes:u64, sha256:String }
fn read(path:&Path, max:u64)->Result<Vec<u8>> {
    ensure_no_reparse_ancestors(path)?;
    ensure!(path.is_file(),"provisioning file unavailable");
    let mut raw=Vec::new();std::fs::File::open(path)?.take(max+1).read_to_end(&mut raw)?;
    ensure!(raw.len() as u64<=max,"provisioning file size bound");Ok(raw)
}
fn installed_file(root:&Path, relative:&str)->Result<PathBuf> {
    let path=Path::new(relative);
    ensure!(!relative.is_empty() && path.components().all(|part|matches!(part,Component::Normal(_))),"invalid installed provisioning path");
    let path=root.join(path);ensure_no_reparse_ancestors(&path)?;
    ensure!(path.is_file(),"installed provisioning file missing");Ok(path)
}
fn verify_asset(root:&Path, asset:&Asset)->Result<()> {
    ensure!(asset.bytes>0 && asset.sha256.len()==64 && asset.sha256.bytes().all(|c|c.is_ascii_digit()||(b'a'..=b'f').contains(&c)),"invalid installed asset identity");
    let path=installed_file(root,&asset.path)?;
    ensure!(path.metadata()?.len()==asset.bytes && digest_file(&path)?==asset.sha256,"installed provisioning asset identity mismatch");Ok(())
}
fn exact_path(actual:&Path, expected:&Path)->Result<()> {
    ensure!(actual.is_absolute() && actual==expected,"generated provisioning path mismatch");
    ensure_no_reparse_ancestors(actual)
}

pub fn validate_generated(result:&ProvisionResult, request:&ProvisionRequest, context:&ProvisioningContext, install_root:&Path)->Result<PathBuf> {
    let facts=&result.metadata;
    ensure!(facts["status"]=="PASS_WORKER_PROVISION" && facts["preflight"]==true,"Worker preflight required");
    ensure!(facts["attempt_id"].as_str()==Some(context.attempt_id()),"Worker result attempt identity differs from Manager context");
    ensure!(!result.private_key().is_empty(),"Worker private key unavailable");
    ensure!(install_root.is_absolute(),"absolute installed prerequisite root required");
    ensure_no_reparse_ancestors(install_root)?;
    let descriptor:Prerequisites=serde_json::from_slice(&read(&install_root.join("provisioning-prerequisites.json"),65536)?)?;
    ensure!(descriptor.schema=="worker-provision-prerequisites/1" && descriptor.asset_version==env!("CARGO_PKG_VERSION"),"installed prerequisite version mismatch");
    let manifest=installed_file(install_root,&descriptor.asset_manifest)?;
    let assets:Assets=serde_json::from_slice(&read(&manifest,65536)?)?;
    ensure!(assets.schema=="worker-provision-assets/1" && assets.version==descriptor.asset_version,"installed assets version mismatch");
    let asset_root=manifest.parent().context("asset manifest parent required")?;
    verify_asset(asset_root,&assets.worker)?;verify_asset(asset_root,&assets.loader)?;
    let known_hosts=installed_file(install_root,&descriptor.known_hosts)?;
    ensure!(!read(&known_hosts,1024*1024)?.is_empty(),"known_hosts is empty");
    let certificate=String::from_utf8(read(&installed_file(install_root,&descriptor.tls_certificate)?,32768)?)?;
    let output=context.output_dir();ensure_no_reparse_ancestors(output)?;
    let gateway_path=output.join("gateway.json");
    ensure!(facts["gateway_path"].as_str().map(Path::new)==Some(gateway_path.as_path()),"Helper output path differs from Manager context");
    for name in ["gateway.json","controller.json","worker.json","model-endpoint.json","protocol.key"] {
        let raw=read(&output.join(name),65536)?;
        for secret in [&request.root_password[..],&request.model_token[..],result.private_key()] {
            ensure!(secret.is_empty() || !raw.windows(secret.len()).any(|part|part==secret),"secret persisted in generated configuration");
        }
    }
    ensure!(!output.join("ordinary-user-rsa").try_exists()?,"SSH private key must not be staged");
    let gateway=SshGatewayConfig::load(&gateway_path)?;
    ensure!(gateway.host==request.host.trim() && facts["host"]==gateway.host && facts["worker_id"]==gateway.worker_id,"Worker request/result identity mismatch");
    ensure!(facts["artifact_sha256"]==assets.worker.sha256 && facts["artifact_bytes"].as_u64()==Some(assets.worker.bytes)
        && gateway.artifact_sha256==assets.worker.sha256 && gateway.artifact_bytes==assets.worker.bytes,"Worker asset identity mismatch");
    exact_path(&gateway.known_hosts,&known_hosts)?;
    exact_path(&gateway.controller_config,&output.join("controller.json"))?;
    exact_path(&gateway.key_file,&output.join("protocol.key"))?;
    exact_path(&gateway.identity_file,&output.join("ordinary-user-rsa"))?;
    exact_path(&gateway.state_dir,&output.join("gateway-state"))?;
    ensure!(gateway.port==22 && gateway.launch_mode==GatewayLaunchMode::DelegatedUserService && gateway.timeout_secs==300
        && gateway.expected_worker_timeout_secs==Some(270),"generated Worker launch contract mismatch");
    let remote_root=gateway.remote_worker.strip_suffix("/aw-cli").context("invalid Worker executable path")?;
    let expected_prefix=format!("/home/{}/cross-runtime-",gateway.user);
    let suffix=remote_root.strip_prefix(&expected_prefix).context("Worker must be staged under discovered user's home")?;
    ensure!(!suffix.is_empty() && suffix.bytes().all(|c|c.is_ascii_alphanumeric()||c==b'-'),"invalid remote staging identity");
    ensure!(gateway.remote_config==format!("{remote_root}/worker.json") && gateway.remote_library_path.as_deref()==Some(format!("{remote_root}/lib").as_str())
        && gateway.remote_library_sha256.as_deref()==Some(assets.loader.sha256.as_str()) && gateway.remote_library_bytes==Some(assets.loader.bytes),"Worker loader/config contract mismatch");
    let controller:Config=serde_json::from_slice(&read(&gateway.controller_config,65536)?)?;controller.validate()?;
    ensure!(controller.mode==Mode::Real && controller.role=="controller" && controller.deployment==gateway.deployment && controller.nodes.len()==1,"generated controller identity mismatch");
    exact_path(&controller.state_dir,&output.join("controller-state"))?;
    ensure!(!controller.state_dir.try_exists()?,"new provisioning must not reopen existing Store");
    ensure!(controller.ao.is_none() && controller.reason_endpoint.is_none() && controller.endpoint.is_none() && controller.runner_node.is_none()
        && controller.controller_address.is_none() && controller.workspace_root.is_none(),"unexpected generated controller endpoints");
    let node=&controller.nodes[0];exact_path(&node.key_file,&output.join("protocol.key"))?;
    ensure!(node.id==gateway.worker_id && node.address==format!("{}:22",gateway.host) && node.slots==1 && node.admitted && !node.healthy && !node.draining
        && node.templates==["sum-v1","control-v1","project-work-v1"],"generated controller node mismatch");
    controller.load_nodes(output)?;
    let worker:NativeWorkerConfig=serde_json::from_slice(&read(&output.join("worker.json"),65536)?)?;
    ensure!(worker.schema=="native-worker-config/1" && worker.deployment==gateway.deployment && worker.worker_id==gateway.worker_id
        && worker.key_file==Path::new("protocol.key") && worker.artifact_sha256==assets.worker.sha256 && worker.timeout_secs==270
        && worker.cgroup_root_mode==CgroupRootMode::CurrentUnit,"generated Linux Worker contract mismatch");
    // A Linux absolute path is not Path::is_absolute on the Windows Manager.
    let cgroup=worker.cgroup_root.to_str().context("invalid Linux cgroup path")?;
    let uid=cgroup.strip_prefix("/sys/fs/cgroup/user.slice/user-").and_then(|s|s.split('.').next()).and_then(|s|s.parse::<u32>().ok()).context("invalid user cgroup")?;
    ensure!((1000..65534).contains(&uid) && cgroup==format!("/sys/fs/cgroup/user.slice/user-{uid}.slice/user@{uid}.service"),"unexpected user cgroup");
    let endpoint=ModelEndpoint::load(&output.join("model-endpoint.json"))?;
    ensure!(endpoint.base_url==request.model_base_url.trim() && endpoint.model==request.model && endpoint.certificate_pem==certificate
        && endpoint.timeout_secs==crate::adapters::model_endpoint::ModelEndpoint::MAX_TIMEOUT_SECS && endpoint.connect_addr.is_none_or(|connect| Some(&connect)==super::provision::resolve_connect_addr(request.model_base_url.trim()).as_ref()),"generated model endpoint differs from trusted input");
    Ok(gateway_path)
}
