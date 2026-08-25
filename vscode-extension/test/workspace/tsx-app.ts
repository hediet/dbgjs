interface RuntimeState {
	count: number;
	message: string;
}

const state: RuntimeState = {
	count: 0,
	message: "executed directly with tsx",
};

setInterval(() => {
	state.count += 1;
	console.log(`${state.message}: ${state.count}`);
	debugger;
}, 250);
