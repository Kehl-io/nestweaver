<template>
  <button @click="refresh">Refresh</button>
</template>

<script>
export async function loadItems(api) {
  const res = await api.get('/items')
  return res.data
}

async function fetchItems() {
  return []
}

export function count(items) {
  return items.length
}

export default {
  methods: {
    refresh() {
      return fetchItems()
    }
  }
}
</script>
