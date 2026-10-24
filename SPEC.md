## Make It So


Coding Agent Orchestration made easy.

The pitch:
- Command a fleet of coding agents, from any provider, from anywhere - via voice, text, mobile app

## Architecture


### The Orchestrator

The orchestrator is the heart (brain) of the operation. It is a running process (mis-orchestrator) that guides one frontier agent. It manages top-level project details, delegates work, and reports back and distills it for integration  with the user.

In essense, the orchestrator process is the user interface that works with the user.

The orchestrator:
- Manages project notes
- Delegates tasks to worker agents
- Reports status, issues, and decisions

The orchestrator has plugins that can be used to visualize pieces (TBD) - but ideally we could build a harness for diagrams, for presentations, for code snippets, and 

The orchestrator is intended to run on a heavier machine, as it will also run the local voice models for parsing and returning text.

Recommended harness + model: Claude + Fable or Codex + Astra

The orchestrator can make decisions on how to allocate tasks based on available harness, available machines and environments, and keep tracking of running tasks and user experiences.

The orchestartor is headless and relies on a 'Voice' (UI) app to communicate and show progress, and to take instructions

### The Workers

The workers are the hands and legs - they do the work. It is a running process (mis-worker) that may or may not be on the same machine. The workers would communicate via an HTTPS protocol or WebSockets

Recommend harness + model: Claude + Opus, Codex + Astra or Sol

We should have an abstract concept of a 'agent' that abstracts out 
  - How to send commands
  - How to get output / streaming output from the agent
  - 

When a worker connects, it should:
- Report information about the machine, available permissions
- The orchestrator should record this ledger

### The Voice

A process that uses WebRTC to connect to the orchestrator.

On desktop, the voice process should use a hotkey. When the hotkey is pressed:
1. A voice channel is opened
2. Voice data is sent, and then a reply is sent back.

On mobile, an app likewise should allow for talking and responses

We should also allow for voice text + responses.

It would also be interesting to support a VR app that allows connecting - this coould show richer visualizations - like an office with agent progress, or a starship bridge with the view screen as the visualization.

The 'voice' apps should also serve as a front-end for showcasing status, showcasing current items in progress, and showcasing images / presentations / etc
  - How to facilitate this? Will they have a bespoke renderer?
  - Or will they stream via  WebRC??

## Technical

The applications should be minimal overhead - based on Rust.

Tauri vs Electron for UI? Web-based is probably the simplest, although I'm open to a native rust framework if it is cross-platform and can render soruce code reasonably. May be different for each platform?

For richer, 3d visualizations - I'd like ot see if we can include 'Functor' for 2d/3d scenes (~/code/functor)


## Other ideas:
- Pluggable visualizations - we should have an abstract data model for the agents, their state, pending work, current work,  current visualizations, etc - so that custom visualizers can be built on top (and 2d vs 3d). A proof of concept would be a 2d office visualize + 3d office visualization and a 2d stgartship visualization (just the viewscreen + helm, first officer, view from captains chair) and a 3d starship visualization (like the bridge of the enterprise)

Workers could be steered by sending orders directly to them, but generally would go through the orchestrator. In the startship example, the orchestrator would be personified by the first officer. 


### Use Cases

- A coding-agent agnostic way to keep tabs on work remotely. For example, at the gym I could monitor what my fleet of codex/claude agents are doing via the mobile app.
- A hands-free way to interact with coding agents while doing other tasks. I could use a lightweight computer (maybe steamdeck + xreal glasses) to keep tabs on whats happening and communicate voice commands/get voice responses and light visuals.
- An immersive - and fun - way to command my fleet of agents in VR. Ideally, we could show some virtual problems and challenges in physical space in vr - leveraging spatial capabilities to assist in solving problems.
