import { Store, processPayment } from "./store.js";

export function main(): string {
  return processPayment(new Store());
}
