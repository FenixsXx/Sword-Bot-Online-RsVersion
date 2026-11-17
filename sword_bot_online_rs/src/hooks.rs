//! Notifiche di cambiamento dei dati, es. per il sito staff in tempo reale
//! (equivalente di `sfo/hooks.py`).
//!
//! Chi vuole sapere che qualcosa è cambiato (`"proposals"`, `"polls"`…) si registra con
//! [`add_site_change_listener`]; i moduli che modificano i dati chiamano [`publish_site_change`].
//! Un ascoltatore che va in panic non impedisce agli altri di ricevere la notifica.
// Verrà usato dal sito staff e dai moduli che pubblicano i cambiamenti.
#![allow(dead_code)]

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::{Arc, LazyLock, Mutex};

use tracing::error;

/// Funzione chiamata a ogni cambiamento, con l'argomento tematico (es. `"proposals"`).
pub type Listener = Arc<dyn Fn(&str) + Send + Sync>;

/// Elenco di ascoltatori. Il programma ne usa uno globale (vedi le funzioni libere sotto);
/// i test ne creano di propri.
#[derive(Default)]
pub struct SiteChangeHub {
    listeners: Mutex<Vec<Listener>>,
}

impl SiteChangeHub {
    pub fn new() -> Self {
        Self::default()
    }

    /// Registra un ascoltatore, una sola volta: lo stesso `Arc` aggiunto due volte non raddoppia
    /// le notifiche (come `callback not in site_change_listeners` in Python).
    pub fn add_listener(&self, listener: Listener) {
        let mut listeners = self.listeners.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        if !listeners.iter().any(|existing| Arc::ptr_eq(existing, &listener)) {
            listeners.push(listener);
        }
    }

    /// Avvisa tutti gli ascoltatori. Il lock non resta preso durante le chiamate, quindi un
    /// ascoltatore può a sua volta registrarne un altro.
    pub fn publish(&self, topic: &str) {
        let listeners: Vec<Listener> =
            self.listeners.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).clone();
        for listener in listeners {
            if catch_unwind(AssertUnwindSafe(|| listener(topic))).is_err() {
                error!("Notifica di cambiamento '{topic}' non riuscita.");
            }
        }
    }

    pub fn len(&self) -> usize {
        self.listeners.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).len()
    }
}

static GLOBAL: LazyLock<SiteChangeHub> = LazyLock::new(SiteChangeHub::new);

pub fn add_site_change_listener(listener: Listener) {
    GLOBAL.add_listener(listener);
}

pub fn publish_site_change(topic: &str) {
    GLOBAL.publish(topic);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn counter() -> (Arc<AtomicUsize>, Listener) {
        let count = Arc::new(AtomicUsize::new(0));
        let seen = Arc::clone(&count);
        (count, Arc::new(move |_topic: &str| {
            seen.fetch_add(1, Ordering::SeqCst);
        }))
    }

    #[test]
    fn listeners_receive_the_topic() {
        let hub = SiteChangeHub::new();
        let topics = Arc::new(Mutex::new(Vec::new()));
        let seen = Arc::clone(&topics);
        hub.add_listener(Arc::new(move |topic: &str| seen.lock().unwrap().push(topic.to_string())));
        hub.publish("proposals");
        hub.publish("polls");
        assert_eq!(*topics.lock().unwrap(), ["proposals", "polls"]);
    }

    #[test]
    fn the_same_listener_is_registered_only_once() {
        let hub = SiteChangeHub::new();
        let (count, listener) = counter();
        hub.add_listener(Arc::clone(&listener));
        hub.add_listener(Arc::clone(&listener));
        assert_eq!(hub.len(), 1);
        hub.publish("x");
        assert_eq!(count.load(Ordering::SeqCst), 1);
        // Un altro ascoltatore, anche se identico nel comportamento, è distinto.
        let (_, other) = counter();
        hub.add_listener(other);
        assert_eq!(hub.len(), 2);
    }

    #[test]
    fn a_panicking_listener_does_not_stop_the_others() {
        let hub = SiteChangeHub::new();
        hub.add_listener(Arc::new(|_topic: &str| panic!("ascoltatore rotto")));
        let (count, listener) = counter();
        hub.add_listener(listener);
        hub.publish("polls");
        hub.publish("polls");
        assert_eq!(count.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn publishing_without_listeners_is_a_no_op() {
        SiteChangeHub::new().publish("nessuno ascolta");
    }

    #[test]
    fn a_listener_may_register_another_one_while_running() {
        let hub = Arc::new(SiteChangeHub::new());
        let (count, late) = counter();
        let inner = Arc::clone(&hub);
        hub.add_listener(Arc::new(move |_topic: &str| inner.add_listener(Arc::clone(&late))));
        hub.publish("a"); // registra `late` senza deadlock
        hub.publish("b");
        assert_eq!(count.load(Ordering::SeqCst), 1, "`late` ascolta solo dalla seconda notifica");
    }
}
