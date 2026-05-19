Object subclass: #STSpyDeepNativeWorkload
    instanceVariableNames: ''
    classVariableNames: ''
    poolDictionaries: ''
    category: 'STSpy'!

!STSpyDeepNativeWorkload class methodsFor: 'primitives'!
deepNativeStack
    <primitive: 'primitiveDeepNativeStack' module: 'STSpyDeepNativePlugin'>
    ^ self error: 'STSpyDeepNativePlugin is not available'!
!

!STSpyDeepNativeWorkload class methodsFor: 'workload'!
spinForSeconds: seconds
    | deadline |
    deadline := Time millisecondClockValue + (seconds * 1000).
    [ Time millisecondClockValue < deadline ] whileTrue: [
        self deepNativeStack.
        Processor yield ]!
!

STSpyDeepNativeWorkload spinForSeconds: 120.
Smalltalk snapshot: false andQuit: true!
