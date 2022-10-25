import { spawn } from 'child_process'
import chalk from 'chalk'
import split from 'split2'
import fs from 'fs'

let CNT = 0
const COLORS = ['red', 'blue', 'green', 'yellow']

main()

function main () {
  const [bin, ...args] = process.argv.slice(2)
  if (!bin) usage()

  let engines = ['node', 'rust']
  let [a, b] = (process.env.MODE || 'node:rust').split(':')
  if ([a, b].map(x => engines.indexOf(x)).filter(x => x !== -1).length !== 2) {
    return usage('MODE must by ENGINE1:ENGINE2 with ENGINE either node or rust')
  }

  const startServer = a === 'node' ? startNode : startRust
  const startClient = b === 'node' ? startNode : startRust

  const serverEnv = {
    // MODE: 'server'
  }
  const clientEnv = {
    // MODE: 'client'
  }
  const procs = []
  const server = startServer(bin, args, { env: serverEnv })
  procs.push(server)
  server.once('stdout-line', line => {
    const client = startClient(bin, args, { env: clientEnv })
    client.stdin.write(line + '\n')
    client.once('stdout-line', line => {
      server.stdin.write(line + '\n')
    })
    procs.push(client)
  })

  process.on('SIGINT', onclose)
}

function onclose () {
  // setTimeout(() => {
  //   procs.forEach(proc => proc.kill())
  //   process.exit()
  // }, 100)
}

function start ({ bin, args, name, color, env = {} }) {
  const proc = spawn(bin, args, {
    env: { ...process.env, ...env }
  })
  console.log(chalk[color].bold(`[${name}] spawn: `) + chalk[color](`${bin} ${args.join(' ')} (pid ${proc.pid})`))
  proc.on('exit', () => {
    console.log(chalk.bold[color](`[${name}] exit ${proc.exitCode}`))
    // onclose()
  })
  proc.stderr.pipe(split()).on('data', line => {
    proc.emit('stderr-line', line)
    console.log(chalk[color](`[${name}]`) + ' ' + line)
  })
  proc.stdout.pipe(split()).on('data', line => {
    proc.emit('stdout-line', line)
    // console.log(chalk[color](`[${name}]`) + ' ' + line)
  })
  return proc
}

function usage (err) {
  if (err) {
    const message = err instanceof Error ? err.message : err
    console.error(message)
  } else {
    const bins = fs.readdirSync('./src-node').map(x => x.replace(/.js$/, ''))
    console.error('USAGE: node run.js <BIN> .[ARGS...]')
    console.error(`where BIN is one of ${bins.join(' , ')}`)
  }
  process.exit(1)
}

function startNode (bin, args = [], opts = {}) {
  if (!opts.name) opts.name = `node${++CNT}`
  if (!opts.color) opts.color = COLORS[CNT % COLORS.length]
  const file = `src-node/${bin}.js`
  const node = start({
    bin: 'node',
    args: [file, ...args],
    ...opts
  })
  return node
}

function startRust (bin, args, opts = {}) {
  if (!opts.name) opts.name = `rust${++CNT}`
  if (!opts.color) opts.color = COLORS[CNT % COLORS.length]
  const cargoArgs = ['run', '--release', '--bin', bin, '--', ...args]
  const rust = start({
    bin: 'cargo',
    args: cargoArgs,
    ...opts
  })
  return rust
}
