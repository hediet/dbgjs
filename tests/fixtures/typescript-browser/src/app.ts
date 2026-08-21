function calculate(value: number): number {
  const doubled = value * 2;
  return doubled + 1;
}

const trigger = document.querySelector<HTMLButtonElement>("#run");
if (!trigger) {
  throw new Error("fixture trigger is missing");
}
trigger.addEventListener("click", () => {
  const result = String(calculate(20));
  trigger.dataset.result = result;
  void fetch(`/result?value=${result}`);
});
