use crate::util::{components::COMPONENT_DATABASE, cursor::CursorContext};

use super::builder::CompletionBuilder;

pub fn complete<'db>(
    context: &'db CursorContext,
    builder: &mut CompletionBuilder<'db>,
) -> Option<()> {
    let range = context.find_environment_name()?;

    for component in COMPONENT_DATABASE.linked_components(&context.project) {
        for name in &component.environments {
            builder.component_environment(range, name, &component.file_names);
        }
    }

    Some(())
}
