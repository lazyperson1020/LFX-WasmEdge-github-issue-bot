use dotenv::dotenv;
use flowsnet_platform_sdk::logger;
use github_flows::{
    event_handler, get_octo, listen_to_event,
    octocrab::models::webhook_events::{WebhookEvent, WebhookEventPayload},
    octocrab::models::webhook_events::payload::IssueCommentWebhookEventAction,
    GithubLogin,
};
use llmservice_flows::{
    chat::ChatOptions,
    LLMServiceFlows,
};
use std::env;

#[no_mangle]
#[tokio::main(flavor = "current_thread")]
pub async fn on_deploy() {
    dotenv().ok();
    logger::init();
    log::info!("Deploying github-issue-handler");

    let owner = env::var("github_owner").expect("github_owner not set");
    let repo = env::var("github_repo").expect("github_repo not set");

    listen_to_event(&GithubLogin::Default, &owner, &repo, vec!["issue_comment"]).await;
}

#[event_handler]
async fn handler(event: Result<WebhookEvent, serde_json::Error>) {
    dotenv().ok();
    logger::init();
    log::info!("Running github-issue-handler handler()");

    let owner = env::var("github_owner").expect("github_owner not set");
    let repo = env::var("github_repo").expect("github_repo not set");
    let trigger_phrase = env::var("trigger_phrase").unwrap_or("@flows_summarize".to_string());
    let llm_api_endpoint = env::var("llm_api_endpoint").expect("llm_api_endpoint not set");
    let llm_model_name = env::var("llm_model_name").unwrap_or("gpt-4".to_string());
    let llm_ctx_size = env::var("llm_ctx_size").unwrap_or("16384".to_string()).parse::<u32>().expect("Invalid llm_ctx_size");
    let llm_api_key = env::var("llm_api_key").expect("llm_api_key not set");

    let payload = match event {
        Ok(payload) => payload,
        Err(e) => {
            log::error!("Error parsing event: {}", e);
            return;
        }
    };

    if let WebhookEventPayload::IssueComment(e) = payload.specific {
        if e.action != IssueCommentWebhookEventAction::Created {
            log::debug!("Ignoring non-created issue comment event");
            return;
        }
        
        let body = e.comment.body.unwrap_or_else(String::new);
        if !body.contains(&trigger_phrase) {
            log::info!("Ignoring comment without trigger phrase");
            return;
        }

        let issue_creator_name = e.issue.user.login;
        let issue_title = e.issue.title;
        let issue_number = e.issue.number;
        let issue_html_url = e.issue.html_url;
        let issue_body = e.issue.body.unwrap_or_default();

        let labels = e.issue.labels.iter().map(|lab| lab.name.clone()).collect::<Vec<String>>().join(", ");
        let mut all_text_from_issue = format!(
            "User '{}', opened an issue titled '{}', labeled '{}', with the following post: '{}'.\n",
            issue_creator_name, issue_title, labels, issue_body
        );

        let octo = get_octo(&GithubLogin::Default);
        let issues = octo.issues(owner.clone(), repo.clone());

        log::debug!("Fetching comments for issue #{}", issue_number);
        let comments = match issues.list_comments(issue_number).per_page(100).send().await {
            Ok(comments_page) => comments_page.items,
            Err(error) => {
                log::error!("Error getting comments from issue: {}", error);
                return;
            }
        };

        for comment in comments {
            let comment_body = comment.body.unwrap_or_else(String::new);
            let commenter = comment.user.login;
            all_text_from_issue.push_str(&format!("{} commented: {}\n", commenter, comment_body));
        }

        log::debug!("Preparing LLM prompts");
        let sys_prompt = format!(
            "Given the information that user '{}' opened an issue titled '{}', your task is to deeply analyze the content of the issue posts. Distill the crux of the issue, the potential solutions suggested.",
            issue_creator_name, issue_title
        );
        
        let co = ChatOptions {
            model: Some(&llm_model_name),
            token_limit: llm_ctx_size,
            restart: true,
            system_prompt: Some(&sys_prompt),
            temperature: Some(0.7),
            max_tokens: Some(192),
            ..Default::default()
        };
        
        let usr_prompt = format!(
            "Analyze the GitHub issue content: {}. Provide a concise analysis touching upon: The central problem discussed in the issue. The main solutions proposed or agreed upon. Aim for a succinct, analytical summary that stays under 128 tokens.",
            all_text_from_issue
        );

        log::debug!("Initializing LLM service");
        let mut llm = LLMServiceFlows::new(&llm_api_endpoint);
        llm.set_api_key(&llm_api_key);
        
        log::debug!("Generating summary with LLM");
        let summary = match llm.chat_completion(&format!("issue_{}", issue_number), &usr_prompt, &co).await {
            Ok(r) => r.choice,
            Err(error) => {
                log::error!("Error generating issue summary #{}: {}", issue_number, error);
                return;
            }
        };

        let resp = format!(
            "{}\n{}\n{}\n\nThis result is generated by flows.network. Triggered by @{}",
            issue_title, issue_html_url, summary, e.comment.user.login
        );
        
        log::debug!("Posting summary comment");
        if let Err(error) = issues.create_comment(issue_number, &resp).await {
            log::error!("Error posting issue summary: {}", error);
        } else {
            log::info!("Successfully posted issue summary for issue #{}", issue_number);
        }
    } else {
        log::warn!("Received non-issue comment event");
    }
}


