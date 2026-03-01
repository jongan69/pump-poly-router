use crate::{
    error::{Result, RouterError},
    types::OrderIntent,
};
use std::collections::HashMap;
use uuid::Uuid;

/// Stores and persists order intents.
///
/// The store keeps orders in memory and optionally writes a JSON snapshot to
/// disk after every mutation so the executor can resume after a restart.
pub struct OrderStore {
    orders: HashMap<Uuid, OrderIntent>,
    /// If set, the store writes a JSON snapshot here on every mutation.
    persist_path: Option<String>,
}

impl OrderStore {
    /// Create an empty in-memory store.
    pub fn new() -> Self {
        OrderStore { orders: HashMap::new(), persist_path: None }
    }

    /// Create a store backed by a JSON file.  Loads existing orders on startup.
    pub fn with_persistence(path: impl Into<String>) -> Result<Self> {
        let path = path.into();
        let orders = if std::path::Path::new(&path).exists() {
            let raw = std::fs::read_to_string(&path)?;
            let list: Vec<OrderIntent> = serde_json::from_str(&raw)?;
            list.into_iter().map(|o| (o.id, o)).collect()
        } else {
            HashMap::new()
        };
        Ok(OrderStore { orders, persist_path: Some(path) })
    }

    /// Insert a new order.  Errors if an order with the same ID already exists.
    pub fn insert(&mut self, order: OrderIntent) -> Result<()> {
        if self.orders.contains_key(&order.id) {
            return Err(RouterError::Other(format!("duplicate order id: {}", order.id)));
        }
        self.orders.insert(order.id, order);
        self.flush()
    }

    /// Replace an existing order (e.g. after a status update).
    pub fn update(&mut self, order: OrderIntent) -> Result<()> {
        self.orders.insert(order.id, order);
        self.flush()
    }

    pub fn get(&self, id: Uuid) -> Option<&OrderIntent> {
        self.orders.get(&id)
    }

    pub fn get_mut(&mut self, id: Uuid) -> Option<&mut OrderIntent> {
        self.orders.get_mut(&id)
    }

    /// IDs of all orders that are not yet in a terminal state.
    pub fn pending_ids(&self) -> Vec<Uuid> {
        self.orders
            .values()
            .filter(|o| !o.status.is_terminal())
            .map(|o| o.id)
            .collect()
    }

    /// All orders (clone, for reporting / debug).
    pub fn all(&self) -> Vec<&OrderIntent> {
        self.orders.values().collect()
    }

    /// Write all orders to the persist file, if configured.
    fn flush(&self) -> Result<()> {
        if let Some(path) = &self.persist_path {
            let list: Vec<&OrderIntent> = self.orders.values().collect();
            let json = serde_json::to_string_pretty(&list)?;
            std::fs::write(path, json)?;
        }
        Ok(())
    }
}

impl Default for OrderStore {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{Outcome, OrderIntent};
    use solana_sdk::pubkey::Pubkey;

    fn make_order() -> OrderIntent {
        OrderIntent::new(
            Pubkey::new_unique(),
            Pubkey::new_unique(),
            1_000_000,
            "cid",
            "tid",
            Outcome::Yes,
            10.0,
            9_999_999_999,
            30,
        )
    }

    #[test]
    fn insert_and_get() {
        let mut store = OrderStore::new();
        let order = make_order();
        let id = order.id;
        store.insert(order).unwrap();
        assert!(store.get(id).is_some());
    }

    #[test]
    fn duplicate_insert_fails() {
        let mut store = OrderStore::new();
        let order = make_order();
        let order2 = order.clone();
        store.insert(order).unwrap();
        assert!(store.insert(order2).is_err());
    }

    #[test]
    fn pending_ids_excludes_terminal() {
        use crate::types::OrderStatus;
        let mut store = OrderStore::new();
        let mut order = make_order();
        let id = order.id;
        order.set_status(OrderStatus::Complete { sol_paid: 0, payout_tx: "tx".into() });
        store.insert(order).unwrap();
        assert!(store.pending_ids().is_empty());
        assert!(store.get(id).is_some());
    }
}
