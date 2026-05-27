<script>
  import { onMount } from 'svelte'
  import Counter from './Counter.svelte'

  export function greet(name) {
    return `Hello, ${name}!`
  }

  export let count = 0

  function handleClick() {
    count += 1
    greet('World')
  }

  onMount(() => {
    console.log('Component mounted')
  })
</script>

<main>
  <h1>Hello {count}</h1>
  <button on:click={handleClick}>Increment</button>
  <Counter />
</main>

<style>
  main {
    padding: 1rem;
  }
</style>
