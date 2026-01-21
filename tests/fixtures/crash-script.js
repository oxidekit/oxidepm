// Script that exits after a specified delay (for testing restart behavior)
const delay = parseInt(process.env.CRASH_DELAY || '1000', 10);

console.log(`Crash script starting, will exit in ${delay}ms`);

setTimeout(() => {
  console.log('Crashing now!');
  process.exit(1);
}, delay);
