import {
  initVM,
  ContractInterfaceFunctionAccess,
} from "@hirosystems/clarinet-sdk";

async function main() {
  const vm = await initVM();
  for (let [contractId, abi] of vm.getContractsInterfaces()) {
    const ast = vm.getContractAST(contractId);
    const source = vm.getContractSource(contractId);
    const [_, contractName] = contractId.split(".");
    if (!["asset", "clarity-bitcoin-mini-deploy"].includes(contractName)) {
      continue;
    }
    console.log(`# ${contractName}`);
    console.log(`## Public Functions`);
    abi.functions
      .filter((f) => f.access === "public")
      .forEach((f) => {
        console.log(`### ${f.name}`);
      });
    console.log(`## Read-only Functions`);
    abi.functions
      .filter((f) => f.access === "read_only")
      .forEach((f) => {
        console.log(`### ${f.name}`);
        console.log("Arguments:");
        console.log(f.args.map((arg) => `${arg.name}: ${JSON.stringify(arg.type)}`).join("\n"));
      });
    console.log();
  }
}

main();
