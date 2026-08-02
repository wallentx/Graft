export class Store {
  get(key: string): string {
    return key;
  }
}

export function processPayment(store: Store): string {
  return store.get("payment");
}
