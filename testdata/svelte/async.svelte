<script context="module">
  export async function load(event) {
    const res = await event.fetch('/api/items')
    return { items: await res.json() }
  }
</script>

<script>
  export let items = []

  export async function refresh() {
    items = await fetchItems()
  }

  async function fetchItems() {
    return []
  }

  export function count() {
    return items.length
  }
</script>

<button on:click={refresh}>Refresh {count()}</button>
