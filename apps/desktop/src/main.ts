import { mount } from 'svelte'
import './msn.css'
import App from './AppMsn.svelte'

const app = mount(App, {
  target: document.getElementById('app')!,
})

export default app
