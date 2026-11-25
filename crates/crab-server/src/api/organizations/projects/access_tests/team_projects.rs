use super::*;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn team_project_links_require_both_roles_and_preserve_stronger_grants() {
    let h = Harness::new(false).await;
    let (organization, _, member) = bootstrap(&h).await;
    let alice = h.login().await;
    *h.provider.mode.lock().await = "member".into();
    let bob = h.login().await;
    let root = format!("/api/organizations/{organization}");
    join(&h, &root, &alice, &bob, member, 0).await;
    let (project, _) = create(&h, &root, &alice).await;
    let team = Uuid::now_v7();
    let teams = format!("{root}/teams/commands");
    let change_team = async |command| {
        h.write(
            Method::POST,
            &teams,
            &alice,
            json!({"request_id":Uuid::now_v7(),"command":command}),
        )
        .await
        .error_for_status()
        .unwrap()
        .json::<Value>()
        .await
        .unwrap()
    };
    assert_eq!(
        h.write(
            Method::POST,
            &teams,
            &alice,
            json!({"request_id":team,"command":{"kind":"create","name":"Secret planning","description":"","visibility":"secret"}})
        ).await.error_for_status().unwrap().json::<Value>().await.unwrap()["outcome"],
        "applied"
    );
    let projects = format!("{root}/projects");
    let linked = format!("{projects}?team={team}");
    let candidates = format!("{linked}&linkable=true&search=release");
    let detail = format!("{projects}/{project}");
    let access = format!("{detail}/access");
    let get_as_bob = async |path: &str| {
        h.http
            .get(format!("{}{path}", h.origin))
            .header("cookie", &bob)
            .send()
            .await
            .unwrap()
    };
    let set = |version, target: Value, role| json!({"kind":"set_access","project":project,"version":version,"target":target,"role":role});
    let link =
        |version| json!({"kind":"link_team","project":project,"version":version,"team":team});
    let try_link = async |version| {
        h.write(
            Method::POST,
            &format!("{projects}/commands"),
            &bob,
            json!({"request_id":Uuid::now_v7(),"command":link(version)}),
        )
        .await
    };
    assert_eq!(get_as_bob(&linked).await.status(), StatusCode::NOT_FOUND);
    assert_eq!(
        h.json(&candidates, &alice).await["items"][0]["id"],
        json!(project)
    );
    assert_eq!(
        change_team(
            json!({"kind":"member","team":team,"member":member,"version":0,"role":"maintainer"})
        )
        .await["outcome"],
        "applied"
    );
    assert_eq!(h.json(&candidates, &bob).await["items"], json!([]));
    assert_eq!(try_link(1).await.status(), StatusCode::FORBIDDEN);
    assert_eq!(
        project_command(
            &h,
            &root,
            &alice,
            set(1, json!({"kind":"member","id":member}), "admin")
        )
        .await["outcome"],
        "applied"
    );
    assert_eq!(
        h.json(&candidates, &bob).await["items"][0]["id"],
        json!(project)
    );
    let submission = json!({"request_id":Uuid::now_v7(),"command":link(2)});
    let save = async || {
        h.write(
            Method::POST,
            &format!("{projects}/commands"),
            &bob,
            submission.clone(),
        )
        .await
        .error_for_status()
        .unwrap()
        .json::<Value>()
        .await
        .unwrap()
    };
    let result = save().await;
    assert_eq!(result["outcome"], "applied", "{result}");
    assert_eq!(save().await, result);
    assert_eq!(
        h.json(&linked, &bob).await["items"][0]["id"],
        json!(project)
    );
    assert_eq!(h.json(&candidates, &bob).await["items"], json!([]));
    assert_eq!(h.json(&access, &alice).await["grants"][1]["role"], "read");
    assert_eq!(
        project_command(
            &h,
            &root,
            &alice,
            set(3, json!({"kind":"team","id":team}), "write")
        )
        .await["outcome"],
        "applied"
    );
    assert_eq!(
        project_command(&h, &root, &alice, link(4)).await["version"],
        4
    );
    assert_eq!(h.json(&access, &alice).await["grants"][1]["role"], "write");
    assert_eq!(
        change_team(
            json!({"kind":"member","team":team,"member":member,"version":1,"role":"member"})
        )
        .await["outcome"],
        "applied"
    );
    assert_eq!(try_link(4).await.status(), StatusCode::FORBIDDEN);
    assert_eq!(
        get_as_bob(&candidates).await.status(),
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        project_command(
            &h,
            &root,
            &alice,
            set(4, json!({"kind":"team","id":team}), "none")
        )
        .await["outcome"],
        "applied"
    );
    assert_eq!(h.json(&linked, &bob).await["items"], json!([]));
    h.close().await;
}
