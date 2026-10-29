export const meta = { name: 'fanout-5', description: 'fan-out orchestration probe', phases: ['fanout', 'reduce'] };

const answers = await phase('fanout', async () =>
  Promise.all(
    Array.from({ length: 5 }, (_, index) => agent(`FIMODE=clean fanout agent ${index}`)),
  ),
);

const summary = await phase('reduce', async () => {
  const joined = answers.map((answer) => String(answer ?? '')).join('|');
  return `agents=${answers.length} chars=${joined.length}`;
});

export default summary;
