Object subclass: #STSpyFFIPluginWorkload
    instanceVariableNames: ''
    classVariableNames: ''
    poolDictionaries: ''
    category: 'STSpy'!

!STSpyFFIPluginWorkload class methodsFor: 'primitives'!
ffiPluginVersion
    <primitive: 'primitivePluginVersion' module: 'SqueakFFIPrims'>
    ^ nil!

cDataModel
    <primitive: 'primitiveCDataModel' module: 'SqueakFFIPrims'>
    ^ nil!
!

!STSpyFFIPluginWorkload class methodsFor: 'workload'!
spinForSeconds: seconds
    | deadline n |
    deadline := Time millisecondClockValue + (seconds * 1000).
    n := 1.
    [ Time millisecondClockValue < deadline ] whileTrue: [
        1 to: 20000 do: [ :i |
            n := n + i.
            i factorial.
            self ffiPluginVersion.
            self cDataModel ].
        Processor yield ].
    ^ n!
!

STSpyFFIPluginWorkload spinForSeconds: 120.
Smalltalk snapshot: false andQuit: true!
