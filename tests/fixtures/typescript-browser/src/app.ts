function applyDiscount(total: number, rate: number): number {
  const discount = total * rate;
  return total - discount;
}

function checkout(items: number[]): number {
  const subtotal = items.reduce((sum, item) => sum + item, 0);
  const finalTotal = applyDiscount(subtotal, 0.1);
  return finalTotal;
}

const trigger = document.querySelector<HTMLButtonElement>("#run");
if (!trigger) {
  throw new Error("fixture trigger is missing");
}
trigger.addEventListener("click", () => {
  const result = String(checkout([12, 18, 20]));
  trigger.dataset.result = result;
  void fetch(`/result?value=${result}`);
});
