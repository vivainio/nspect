use std::{fs, process::Command, time::SystemTime};

use dotnetdll::prelude::*;
use serde_json::json;

#[test]
fn reports_direct_calls_using_restored_or_explicit_assemblies() {
    let root = std::env::temp_dir().join(format!(
        "nspect-usages-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let packages = root.join("packages");
    let dll = packages.join("nservicebus/1.0/ref/net8.0/NServiceBus.dll");
    fs::create_dir_all(dll.parent().unwrap()).unwrap();
    fs::create_dir(root.join("obj")).unwrap();

    let mut assembly = Resolution::new(Module::new("NServiceBus.dll"));
    assembly.assembly = Some(Assembly::new("NServiceBus"));
    assembly.push_type_definition(TypeDefinition::new(
        Some("NServiceBus".into()),
        "IMessageSession",
    ));
    fs::write(&dll, assembly.write(WriteOptions::default()).unwrap()).unwrap();
    let project = root.join("App.csproj");
    fs::write(&project, r#"<Project Sdk="Microsoft.NET.Sdk"/>"#).unwrap();
    fs::write(
        root.join("App.cs"),
        r#"using NServiceBus;
using Session = NServiceBus.IMessageSession;
class App {
    private IMessageSession session;
    private Session Property { get; }
    void Send(IMessageSession parameter, Other other) {
        parameter.Send();
        this.session.Publish();
        Property.Send();
        NServiceBus.IMessageSession local = parameter;
        local.Send();
        other.Send();
        { Other session = other; session.Send(); }
    }
}
"#,
    )
    .unwrap();
    let assets = root.join("obj/project.assets.json");
    fs::write(
        &assets,
        serde_json::to_vec(&json!({
            "targets": {"net8.0": {"NServiceBus/1.0": {
                "type": "package",
                "compile": {"ref/net8.0/NServiceBus.dll": {}}
            }}},
            "libraries": {"NServiceBus/1.0": {"path": "nservicebus/1.0"}},
            "packageFolders": {(packages.to_str().unwrap()): {}}
        }))
        .unwrap(),
    )
    .unwrap();

    let run = |explicit: bool| {
        let mut command = Command::new(env!("CARGO_BIN_EXE_nspect"));
        command.arg("usages").arg(&root).arg("NServiceBus");
        if explicit {
            command.arg("--dll").arg(&dll);
        }
        command.output().unwrap()
    };
    let restored = run(false);
    fs::remove_file(assets).unwrap();
    let explicit = run(true);
    fs::write(
        &project,
        r#"<Project><ItemGroup>
            <Reference Include="NServiceBus">
                <HintPath>packages\nservicebus\1.0\ref\net8.0\NServiceBus.dll</HintPath>
            </Reference>
            <Compile Include="App.cs"/>
            <Reference Include="Another"/>
        </ItemGroup></Project>"#,
    )
    .unwrap();
    let hinted = run(false);
    fs::remove_dir_all(root).unwrap();

    let expected = concat!(
        "App.cs:7:19  NServiceBus.IMessageSession  Send\n",
        "App.cs:8:22  NServiceBus.IMessageSession  Publish\n",
        "App.cs:9:18  NServiceBus.IMessageSession  Send\n",
        "App.cs:11:15  NServiceBus.IMessageSession  Send\n",
    );
    for output in [restored, explicit, hinted] {
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(String::from_utf8(output.stdout).unwrap(), expected);
    }
}
