use alloc::{collections::BTreeMap, string::String, vec::Vec};

#[derive(Clone, PartialEq)]
pub enum ServiceState { Pending, Running, Failed, Counterfactual }

pub struct CausalService {
    pub name: String,
    pub state: ServiceState,
    pub causes: Vec<String>,           // must be Running to trigger this
    pub counterfactual: Option<String>,// fallback if this fails
    pub do_probability: f64,           // P(do(service)) — Bayesian belief it succeeds
}

pub struct CausalBootDag {
    services: BTreeMap<String, CausalService>,
}

impl CausalBootDag {
    pub fn new() -> Self { Self { services: BTreeMap::new() } }

    pub fn register(&mut self, svc: CausalService) {
        self.services.insert(svc.name.clone(), svc);
    }

    /// Advance the DAG — fire all services whose causes are satisfied
    pub fn tick(&mut self) -> Vec<String> {
        let mut fired = Vec::new();
        let names: Vec<String> = self.services.keys().cloned().collect();

        for name in &names {
            let causes_met = {
                let svc = &self.services[name];
                svc.state == ServiceState::Pending &&
                svc.causes.iter().all(|dep| {
                    self.services.get(dep)
                        .map(|d| d.state == ServiceState::Running)
                        .unwrap_or(false)
                })
            };

            if causes_met {
                let svc = self.services.get_mut(name).unwrap();
                // Bayesian launch: high-confidence services start immediately
                if svc.do_probability > 0.5 {
                    svc.state = ServiceState::Running;
                    fired.push(name.clone());
                }
            }
        }

        // Counterfactual intervention — if something failed, activate its fallback
        for name in &names {
            let (failed, fallback) = {
                let svc = &self.services[name];
                (svc.state == ServiceState::Failed, svc.counterfactual.clone())
            };
            if failed {
                if let Some(fb) = fallback {
                    if let Some(fb_svc) = self.services.get_mut(&fb) {
                        fb_svc.state = ServiceState::Counterfactual;
                        fired.push(fb.clone());
                    }
                }
            }
        }
        fired
    }
}
