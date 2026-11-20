#[path="../src/desktop/mod.rs"]
mod desktop;

use agentranch::core::service::{validate_provision_input, ProvisionRequest};

#[test]
fn rejects_invalid_onboarding_fields_before_helper_start() {
    let cases = [
        ("", "root", "https://model.example", "model", "host"),
        ("172.28.8.221", "", "https://model.example", "model", "root_user"),
        ("172.28.8.221", "root", "http://model.example", "model", "model_base_url"),
        ("172.28.8.221", "root", "https://model.example/v1", "model", "model_base_url"),
        ("172.28.8.221", "root", "https://model.example", "", "model"),
    ];
    for (host, root_user, base_url, model, expected) in cases {
        let request = ProvisionRequest {
            host: host.into(),
            root_user: root_user.into(),
            root_password: b"root-secret".to_vec(),
            model_base_url: base_url.into(),
            model_token: b"api-secret".to_vec(),
            model: model.into(),
        };
        let error = validate_provision_input(&request).unwrap_err();
        assert!(error.to_string().contains(expected), "{expected}: {error:#}");
    }
}

#[test]
fn provision_debug_redacts_secret_bytes() {
    let request = ProvisionRequest {
        host: "172.28.8.221".into(),
        root_user: "root".into(),
        root_password: b"root-secret".to_vec(),
        model_base_url: "https://model.example".into(),
        model_token: b"api-secret".to_vec(),
        model: "default-model".into(),
    };
    let debug = format!("{request:?}");
    assert!(debug.contains("root_password_len"));
    assert!(debug.contains("model_token_len"));
    assert!(!debug.contains("root-secret"));
    assert!(!debug.contains("api-secret"));
}

#[test]
fn onboarding_validation_errors_map_to_safe_field_guidance() {
    let invalid_host = ProvisionRequest {
        host: "not-an-ip".into(),
        root_user: "root".into(),
        root_password: b"synthetic-password".to_vec(),
        model_base_url: "https://model.example".into(),
        model_token: b"synthetic-token".to_vec(),
        model: "test-model".into(),
    };
    let host_error = validate_provision_input(&invalid_host).unwrap_err();
    let host_copy = desktop::onboarding_input_error_copy(&host_error);
    assert!(host_copy.contains("Linux 工作节点 IP"));
    assert!(host_copy.contains("IPv4"));

    for base_url in ["http://model.example", "https://model.example/v1", "https://:8443"] {
        let invalid_origin = ProvisionRequest {
            host: "192.0.2.10".into(),
            root_user: "root".into(),
            root_password: b"synthetic-password".to_vec(),
            model_base_url: base_url.into(),
            model_token: b"synthetic-token".to_vec(),
            model: "test-model".into(),
        };
        let origin_error = validate_provision_input(&invalid_origin).unwrap_err();
        let origin_copy = desktop::onboarding_input_error_copy(&origin_error);
        assert!(origin_copy.contains("第三方服务地址"), "{base_url}: {origin_copy}");
        assert!(origin_copy.contains("HTTPS"), "{base_url}: {origin_copy}");
    }

    let missing_password = ProvisionRequest {
        host: "192.0.2.10".into(),
        root_user: "root".into(),
        root_password: Vec::new(),
        model_base_url: "https://model.example".into(),
        model_token: b"synthetic-token".to_vec(),
        model: "test-model".into(),
    };
    let password_copy = desktop::onboarding_input_error_copy(
        &validate_provision_input(&missing_password).unwrap_err(),
    );
    assert!(password_copy.contains("root 密码"));

    let missing_token = ProvisionRequest {
        host: "192.0.2.10".into(),
        root_user: "root".into(),
        root_password: b"synthetic-password".to_vec(),
        model_base_url: "https://model.example".into(),
        model_token: Vec::new(),
        model: "test-model".into(),
    };
    let token_copy = desktop::onboarding_input_error_copy(
        &validate_provision_input(&missing_token).unwrap_err(),
    );
    assert!(token_copy.contains("API key"));

    let missing_model = ProvisionRequest {
        host: "192.0.2.10".into(),
        root_user: "root".into(),
        root_password: b"synthetic-password".to_vec(),
        model_base_url: "https://model.example".into(),
        model_token: b"synthetic-token".to_vec(),
        model: String::new(),
    };
    let model_copy = desktop::onboarding_input_error_copy(
        &validate_provision_input(&missing_model).unwrap_err(),
    );
    assert!(model_copy.contains("默认模型"));

    for copy in [host_copy, password_copy, token_copy, model_copy] {
        assert!(!copy.contains("synthetic-password"));
        assert!(!copy.contains("synthetic-token"));
    }
}

#[test]
fn unknown_onboarding_validation_errors_use_generic_safe_copy() {
    let error = anyhow::anyhow!("secret diagnostic synthetic-password synthetic-token");
    let copy = desktop::onboarding_input_error_copy(&error);
    assert_eq!(copy, "输入未通过校验，请检查六项信息后重试");
    assert!(!copy.contains("synthetic-password"));
    assert!(!copy.contains("synthetic-token"));
}

#[test]
fn windows_resource_notice_is_not_a_remote_failure() {
    assert_eq!(
        agentranch::core::service::classify_windows_resource_notice("low_memory"),
        "TEST_ENV_RESOURCE_LIMIT"
    );
    assert_ne!(
        agentranch::core::service::classify_windows_resource_notice("low_memory"),
        "real_not_ready"
    );
}
