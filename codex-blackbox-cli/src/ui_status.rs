use serde::Serialize;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum UiConfigState {
    NotConfigured,
    Configured,
    Misconfigured,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum UiStatusState {
    NotConfigured,
    ConfiguredUnobserved,
    HttpResponsesUnparsed,
    WebsocketOnlyUnobservable,
    ObservingRecentUiTraffic,
    StaleObservedTrafficOnly,
    LikelyMisconfiguredOrAppServerNotRestarted,
}

#[derive(Clone, Debug)]
pub(crate) struct UiStatusInput {
    pub(crate) config_state: UiConfigState,
    pub(crate) observed_codex_responses: bool,
    pub(crate) latest_observed_age_seconds: Option<u64>,
    pub(crate) recent_http_responses_post: bool,
    pub(crate) recent_websocket_upgrade_required: bool,
    pub(crate) active_app_server_processes: bool,
    pub(crate) recent_threshold_seconds: u64,
}

pub(crate) fn classify(input: &UiStatusInput) -> UiStatusState {
    match input.config_state {
        UiConfigState::NotConfigured => UiStatusState::NotConfigured,
        UiConfigState::Misconfigured => UiStatusState::LikelyMisconfiguredOrAppServerNotRestarted,
        UiConfigState::Configured => {
            if !input.observed_codex_responses {
                if input.recent_http_responses_post {
                    UiStatusState::HttpResponsesUnparsed
                } else if input.recent_websocket_upgrade_required {
                    UiStatusState::WebsocketOnlyUnobservable
                } else if input.active_app_server_processes {
                    UiStatusState::LikelyMisconfiguredOrAppServerNotRestarted
                } else {
                    UiStatusState::ConfiguredUnobserved
                }
            } else if input
                .latest_observed_age_seconds
                .is_some_and(|age| age <= input.recent_threshold_seconds)
            {
                UiStatusState::ObservingRecentUiTraffic
            } else {
                UiStatusState::StaleObservedTrafficOnly
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{classify, UiConfigState, UiStatusInput, UiStatusState};

    fn input(config_state: UiConfigState) -> UiStatusInput {
        UiStatusInput {
            config_state,
            observed_codex_responses: false,
            latest_observed_age_seconds: None,
            recent_http_responses_post: false,
            recent_websocket_upgrade_required: false,
            active_app_server_processes: false,
            recent_threshold_seconds: 900,
        }
    }

    #[test]
    fn status_classification_distinguishes_required_ui_states() {
        assert_eq!(
            classify(&input(UiConfigState::NotConfigured)),
            UiStatusState::NotConfigured
        );

        assert_eq!(
            classify(&input(UiConfigState::Configured)),
            UiStatusState::ConfiguredUnobserved
        );

        let mut recent = input(UiConfigState::Configured);
        recent.observed_codex_responses = true;
        recent.latest_observed_age_seconds = Some(60);
        assert_eq!(classify(&recent), UiStatusState::ObservingRecentUiTraffic);

        let mut stale = input(UiConfigState::Configured);
        stale.observed_codex_responses = true;
        stale.latest_observed_age_seconds = Some(3600);
        assert_eq!(classify(&stale), UiStatusState::StaleObservedTrafficOnly);

        let mut active_unobserved = input(UiConfigState::Configured);
        active_unobserved.active_app_server_processes = true;
        assert_eq!(
            classify(&active_unobserved),
            UiStatusState::LikelyMisconfiguredOrAppServerNotRestarted
        );

        let mut websocket_unobserved = input(UiConfigState::Configured);
        websocket_unobserved.active_app_server_processes = true;
        websocket_unobserved.recent_websocket_upgrade_required = true;
        assert_eq!(
            classify(&websocket_unobserved),
            UiStatusState::WebsocketOnlyUnobservable
        );

        let mut http_unparsed = input(UiConfigState::Configured);
        http_unparsed.active_app_server_processes = true;
        http_unparsed.recent_http_responses_post = true;
        assert_eq!(
            classify(&http_unparsed),
            UiStatusState::HttpResponsesUnparsed
        );

        let mut both_http_and_websocket = input(UiConfigState::Configured);
        both_http_and_websocket.recent_http_responses_post = true;
        both_http_and_websocket.recent_websocket_upgrade_required = true;
        assert_eq!(
            classify(&both_http_and_websocket),
            UiStatusState::HttpResponsesUnparsed
        );

        assert_eq!(
            classify(&input(UiConfigState::Misconfigured)),
            UiStatusState::LikelyMisconfiguredOrAppServerNotRestarted
        );
    }
}
