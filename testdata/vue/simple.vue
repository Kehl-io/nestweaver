<template>
  <div class="greeter">
    <h1>{{ greeting }}</h1>
    <button @click="handleClick">Click me</button>
  </div>
</template>

<script>
import { ref, computed } from 'vue'
import HelperUtil from './utils/helper'

export function formatName(name) {
  return name.trim().toUpperCase()
}

export default defineComponent({
  name: 'SimpleGreeter',
  props: {
    name: String
  },
  setup(props) {
    const greeting = ref(`Hello, ${props.name}!`)
    const reversed = computed(() => greeting.value.split('').reverse().join(''))

    function handleClick() {
      console.log(formatName(props.name))
    }

    return { greeting, reversed, handleClick }
  }
})
</script>

<style scoped>
.greeter {
  padding: 1rem;
}
</style>
