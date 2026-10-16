---
name: slides
description: Make a new Slides artifact in Tickloop Artifacts about a topic.
disable-model-invocation: true
---

# New Slides

The user ran /slides to make a new Slides artifact. Topic: $ARGUMENTS

1. When no topic follows "Topic:", ask the user with AskUserQuestion what these Slides should present, and continue with their answer. Ask the same way for anything else you must settle with the user before writing, such as the audience or the length, so every answer comes back while you work on these Slides.
2. While Tickloop Artifacts is signed out, call ArtifactLogin and give the user its link and code; then ask with AskUserQuestion whether they have signed in, and continue once the Artifact tools are available.
3. Before writing any Slide, load `tickloop-artifacts:artifact-slides` with the Skill tool and follow it.
4. Create the Slides with ArtifactEdit (`action: "create"`, `format: "slides"`) and write them through to the end as that guidance describes. To show a local image, upload it with ArtifactUpload and use the `asset:` reference it returns.
5. Finish by giving the user the URL of the Slides.
