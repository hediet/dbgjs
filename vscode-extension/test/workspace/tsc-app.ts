interface CompiledState {
	count: number;
	message: string;
}

const state: CompiledState = {
	count: 0,
	message: "compiled with tsc",
};

setInterval(() => {
	state.count += 1;
	console.log(`${state.message}: ${state.count}`);
	debugger;
}, 250);
