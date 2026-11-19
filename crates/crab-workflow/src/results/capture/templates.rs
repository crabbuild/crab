use super::ResultCapture;
use crate::results::{
    Attachment, AttachmentKind, Availability, MAX_ATTACHMENT_BYTES, MAX_ATTACHMENTS,
    MAX_CAPTURE_BYTES, digest, invalid, local::read_bounded,
    plot_templates::resolve_custom_plot_template_path,
};
use crate::{Result, WorkflowError};
use std::path::{Component, Path};

impl ResultCapture {
    pub(super) fn capture_templates(&mut self) -> Result<()> {
        for config in &mut self.workflow.plot_configs {
            let Some(template) = &config.template else {
                continue;
            };
            let Some(source) = resolve_custom_plot_template_path(&self.template_root, template)
            else {
                continue;
            };
            let admitted = if self.attachments.len() >= MAX_ATTACHMENTS {
                Err(invalid("Attachment limit exceeded"))
            } else {
                validate_source(&self.template_root, &source)
            };
            let limit =
                MAX_ATTACHMENT_BYTES.min(MAX_CAPTURE_BYTES.saturating_sub(self.captured_bytes));
            let bytes = admitted.and_then(|()| read_bounded(&source, limit));
            let attachment = match bytes {
                Ok(bytes) => {
                    self.captured_bytes += bytes.len();
                    let hash = self.local.put_blob(&bytes)?;
                    Attachment {
                        path: format!("templates/{hash}.json"),
                        kind: AttachmentKind::Template,
                        stage: None,
                        hash: Some(hash),
                        size: bytes.len() as u64,
                        state: Availability::Present,
                        reason: None,
                        push: true,
                        remote: None,
                    }
                }
                Err(error) => {
                    let reason = format!("Template {template}: {error}");
                    let constraint = match &error {
                        WorkflowError::Configuration { origin, .. } => origin.clone(),
                        _ => error.to_string(),
                    };
                    // Capture issues guide recovery in every Results client;
                    // the attachment retains the full diagnostic for inspection.
                    self.issues.push(format!(
                        "Template {template} was not retained: {constraint}. Correct the template and save a new observation with crab exp save, or run a new experiment."
                    ));
                    let state = match &error {
                        WorkflowError::Io(source)
                            if source.kind() == std::io::ErrorKind::PermissionDenied =>
                        {
                            Availability::Denied
                        }
                        WorkflowError::Io(source)
                            if source.kind() == std::io::ErrorKind::NotFound =>
                        {
                            Availability::Missing
                        }
                        _ => Availability::NotCaptured,
                    };
                    Attachment {
                        path: format!("templates/unavailable/{}.json", digest(template.as_bytes())),
                        kind: AttachmentKind::Template,
                        stage: None,
                        hash: None,
                        size: 0,
                        state,
                        reason: Some(reason),
                        push: true,
                        remote: None,
                    }
                }
            };
            // A failed custom capture must stay distinct from built-ins;
            // otherwise a same-name built-in can replace authored semantics.
            config.template = Some(attachment.path.clone());
            if self.attachments.len() < MAX_ATTACHMENTS {
                self.attachments.insert(attachment.path.clone(), attachment);
            }
        }
        Ok(())
    }
}

fn validate_source(root: &Path, source: &Path) -> Result<()> {
    let canonical = source.canonicalize()?;
    // Isolated trees themselves live in private Crab state. Check resources
    // below their root; admitting private repository paths would publish state.
    let relative = canonical.strip_prefix(root).unwrap_or(&canonical);
    let mut components = relative.components();
    while let Some(component) = components.next() {
        if matches!(component, Component::Normal(name) if name.as_encoded_bytes().eq_ignore_ascii_case(b".git"))
            || matches!(component, Component::Normal(name) if name.as_encoded_bytes().eq_ignore_ascii_case(b".crab"))
                && !matches!(components.next(), Some(Component::Normal(name)) if name.as_encoded_bytes().eq_ignore_ascii_case(b"plots"))
        {
            return Err(invalid(
                "Private repository state is not a render asset; put declared templates under .crab/plots or a normal repository path",
            ));
        }
    }
    Ok(())
}
