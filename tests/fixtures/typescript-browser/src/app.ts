class CheckoutService {
  applyDiscount(total: number, rate: number): number {
    const discount = total * rate;
    return total - discount;
  }

  checkout(items: number[]): number {
    const subtotal = items.reduce((sum, item) => sum + item, 0);
    const finalTotal = this.applyDiscount(subtotal, 0.1);
    return finalTotal;
  }
}

const checkoutService = new CheckoutService();
const trigger = document.querySelector<HTMLButtonElement>("#run");
if (!trigger) {
  throw new Error("fixture trigger is missing");
}
trigger.addEventListener("click", () => {
  const result = String(checkoutService.checkout([12, 18, 20]));
  trigger.dataset.result = result;
  void fetch(`/result?value=${result}`);
});
