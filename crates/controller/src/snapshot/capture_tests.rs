use super::*;

#[test]
fn capture_job_names_are_bounded_and_keep_the_full_snapshot_identity() {
    let resolved = kopiur_api::common::resolve_mover(None, None, None, None, None, None);
    let mut names = std::collections::BTreeSet::new();
    for snapshot in [
        "db-1".to_string(),
        "s".repeat(57),
        "s".repeat(58),
        "s".repeat(63),
        "s".repeat(64),
        "s".repeat(253),
        "native-all-sources-daily-native-bundle-20261021221608-long-name-a".to_string(),
        "native-all-sources-daily-native-bundle-20261021221608-long-name-b".to_string(),
        format!("{}.{}", "s".repeat(39), "s".repeat(43)),
    ] {
        let name = crate::naming::capped_job_name(&snapshot);
        assert!(
            names.insert(name.clone()),
            "distinct snapshots must not alias"
        );
        assert_eq!(name, crate::naming::capped_job_name(&snapshot));
        if snapshot.len() <= kopiur_api::consts::JOB_NAME_MAX {
            assert_eq!(name, snapshot, "short names stay compatible");
        }
        let job = render_mover_job_for_snapshot(BTreeMap::new(), &resolved, &snapshot, &name);
        assert_eq!(job.name_any(), name);
        assert!(name.len() <= kopiur_api::consts::JOB_NAME_MAX);
        assert!(format!("{name}-pgt-0").len() <= 63);
        let owner = &job.metadata.owner_references.as_ref().unwrap()[0];
        assert_eq!(owner.name, snapshot);
        assert_eq!(owner.uid, "uid-1");
        let pod = job.spec.as_ref().unwrap().template.spec.as_ref().unwrap();
        let env = pod.containers[0].env.as_ref().unwrap();
        let work = env.iter().find(|e| e.name == "KOPIUR_WORK_SPEC").unwrap();
        let work: serde_json::Value = serde_json::from_str(work.value.as_ref().unwrap()).unwrap();
        assert_eq!(work["targetRef"]["name"], snapshot);
        for labels in [rendered_labels(&job), rendered_pod_labels(&job)] {
            assert!(labels.values().all(|v| v.len() <= 63));
        }
    }
}

async fn capture_lookup_case(snapshot: &str, existing: Option<&str>, expected: &[String]) {
    use http::{Request, Response, StatusCode};
    use kube::client::Body;
    let requests = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let log = requests.clone();
    let existing = existing.map(str::to_string);
    let wanted = existing.clone();
    let svc = tower::service_fn(move |req: Request<Body>| {
        let log = log.clone();
        let existing = existing.clone();
        async move {
            assert_eq!(req.method(), http::Method::GET);
            let name = req.uri().path().rsplit('/').next().unwrap().to_string();
            log.lock().unwrap().push(name.clone());
            let (status, body) = if existing.as_deref() == Some(&name) {
                (
                    StatusCode::OK,
                    serde_json::json!({
                        "apiVersion": "batch/v1", "kind": "Job",
                        "metadata": { "name": name },
                        "status": { "active": 1 }
                    }),
                )
            } else {
                (
                    StatusCode::NOT_FOUND,
                    serde_json::json!({
                        "apiVersion": "v1", "kind": "Status", "status": "Failure",
                        "reason": "NotFound", "code": 404
                    }),
                )
            };
            Ok::<_, std::convert::Infallible>(
                Response::builder()
                    .status(status)
                    .header("content-type", "application/json")
                    .body(Body::from(serde_json::to_vec(&body).unwrap()))
                    .unwrap(),
            )
        }
    });
    let jobs = Api::<Job>::namespaced(kube::Client::new(svc, "prod"), "prod");
    let job = capture_job(&jobs, snapshot).await.unwrap();
    assert_eq!(job.as_ref().map(ResourceExt::name_any), wanted);
    assert_eq!(*requests.lock().unwrap(), expected);
    if job.is_some() {
        assert!(
            !staged_teardown_ready(job.as_ref()),
            "active captures keep staged data"
        );
    }
}

#[tokio::test]
async fn capture_job_lookup_matches_creation_and_preserves_legacy_inflight_jobs() {
    for len in [4, 57, 58, 63, 64, 73, 75, 80, 83, 253] {
        let snapshot = "s".repeat(len);
        let current = crate::naming::capped_job_name(&snapshot);
        let legacy = crate::naming::legacy_job_name(&snapshot, &current);
        let mut expected = legacy.iter().cloned().collect::<Vec<_>>();
        expected.push(current.clone());
        capture_lookup_case(&snapshot, Some(&current), &expected).await;
        capture_lookup_case(&snapshot, None, &expected).await;
        if let Some(legacy) = legacy {
            capture_lookup_case(&snapshot, Some(&legacy), std::slice::from_ref(&legacy)).await;
        }
    }
}
