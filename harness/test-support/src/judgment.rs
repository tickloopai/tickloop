use std::{collections::BTreeMap, sync::Mutex};
use tickloop_harness_model::judgment::{
    Answer, Evaluation, EvaluationRequest, JudgmentClient, JudgmentError, Question,
};
use tickloop_harness_protocol::Usage;
use tokio_util::sync::CancellationToken;
pub struct RecordingJudgment {
    pub requests: Mutex<Vec<EvaluationRequest>>,
}

#[async_trait::async_trait]
impl JudgmentClient for RecordingJudgment {
    fn model(&self) -> &str {
        "fixture-jev"
    }

    async fn evaluate(
        &self,
        request: &EvaluationRequest,
        _: CancellationToken,
    ) -> Result<Evaluation, JudgmentError> {
        self.requests.lock().unwrap().push(request.clone());
        let answers = request
            .questions
            .iter()
            .map(|(id, question)| {
                let Question::Choice { criteria, .. } = question else {
                    panic!("Choice expected")
                };
                let selected = "allow";
                (
                    id.clone(),
                    Answer::Choice {
                        choice: selected.into(),
                        confidence: 1.0,
                        probabilities: criteria
                            .keys()
                            .map(|option| (option.clone(), f64::from(option == selected)))
                            .collect::<BTreeMap<_, _>>(),
                    },
                )
            })
            .collect();
        Ok(Evaluation {
            model: "fixture-jev".into(),
            answers,
            usage: Usage::default(),
        })
    }
}
